use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::MutationSafety;
use crate::config::{self, Options};
use crate::detections::{MutationDetectionMatrix, print_mutation_detection_matrix};
use crate::flakiness::{MutationFlakinessMatrix, print_mutation_flakiness_epilogue, print_mutation_flakiness_matrix};
use crate::metadata::{MutantMeta, MutationMeta, SubstLocIdx, SubstMap, SubstMeta};
use crate::test_runner;
use crate::thread_pool::ThreadPool;

mod test {
    #![allow(unused_imports)]

    pub use ::test::*;
    pub use ::test::test::*;
}

/// Handle storing the currently active substitution map of a program.
///
/// An instance of this handle is automatically created and referenced in
/// meta-mutant programs generated by mutest-rs.
///
/// # Safety
///
/// All valid uses of this handle must be stored in a `static` (or any pinned memory location).
///
/// Reading from this handle remains valid even as the substitution map stored in the handle
/// is changed or swapped out, as it represents a static memory location.
/// For example, it is considered valid for a read from the handle to return substitution metadata
/// for new substitution maps if the handle is simultaneously modified from another thread.
pub struct ActiveMutantHandle<S: SubstMap>(Cell<Option<S>>);

impl<S: SubstMap> ActiveMutantHandle<S> {
    pub const fn empty() -> Self {
        Self(Cell::new(None))
    }

    pub const fn with(v: S) -> Self {
        Self(Cell::new(Some(v)))
    }

    #[inline]
    pub fn subst_at(self: &'static Self, subst_loc_idx: SubstLocIdx) -> Option<SubstMeta> {
        // SAFETY: We are acquiring a reference to the static memory location backing the handle
        //         and the value is allowed to change before the substitution metadata is read.
        let subst_map_ref = (unsafe { &*self.0.as_ptr() }).as_ref();

        subst_map_ref.and_then(|subst| subst.subst_at(subst_loc_idx))
    }

    /// # Safety
    ///
    /// The substitution location index must be valid for the active substitution map.
    #[inline]
    pub unsafe fn subst_at_unchecked(self: &'static Self, subst_loc_idx: SubstLocIdx) -> Option<SubstMeta> {
        // SAFETY: We are acquiring a reference to the static memory location backing the handle
        //         and the value is allowed to change before the substitution metadata is read.
        let subst_map_ref = (unsafe { &*self.0.as_ptr() }).as_ref();

        subst_map_ref.and_then(|subst| subst.subst_at_unchecked(subst_loc_idx))
    }

    /// # Safety
    ///
    /// The caller must ensure that no other thread is reading from the handle.
    pub(crate) unsafe fn replace(&self, v: Option<S>) {
        self.0.replace(v);
    }
}

// SAFETY: While access to the handle data is not synchronized, the handle can only be mutated using
//         unsafe, crate-private functions, see above.
unsafe impl<S: SubstMap> Sync for ActiveMutantHandle<S> {}

const ERROR_EXIT_CODE: i32 = 101;

fn make_owned_test_fn(test_fn: &test::TestFn) -> test::TestFn {
    match test_fn {
        test::TestFn::StaticTestFn(f) => test::TestFn::StaticTestFn(*f),
        test::TestFn::StaticBenchFn(f) => test::TestFn::StaticBenchFn(*f),
        _ => panic!("non-static tests passed to mutest_runtime::mutest_main"),
    }
}

fn make_owned_test_def(test: &test::TestDescAndFn) -> test::TestDescAndFn {
    test::TestDescAndFn {
        desc: test.desc.clone(),
        testfn: make_owned_test_fn(&test.testfn),
    }
}

fn clone_tests(tests: &[test_runner::Test]) -> Vec<test_runner::Test> {
    tests.iter()
        .map(|test| {
            test_runner::Test {
                desc: test.desc.clone(),
                test_fn: make_owned_test_fn(&test.test_fn),
                timeout: test.timeout,
            }
        })
        .collect()
}

struct ProfiledTest {
    pub test: test::TestDescAndFn,
    pub result: test_runner::TestResult,
    pub exec_time: Option<Duration>,
}

fn profile_tests(tests: Vec<test::TestDescAndFn>) -> Result<Vec<ProfiledTest>, Infallible> {
    let tests_to_run = tests.iter()
        .map(|test| {
            test_runner::Test {
                desc: test.desc.clone(),
                test_fn: make_owned_test_fn(&test.testfn),
                timeout: None,
            }
        })
        .collect::<Vec<_>>();

    let mut profiled_tests = Vec::<ProfiledTest>::with_capacity(tests.len());
    let mut remaining_tests = tests;

    let on_test_event = |event, _remaining_tests: &mut Vec<(test::TestId, test_runner::Test)>| -> Result<_, Infallible> {
        match event {
            test_runner::TestEvent::Result(test) => {
                let test_desc_and_fn = remaining_tests
                    .extract_if(|t| t.desc.name == test.desc.name)
                    .next().expect("completed test not found amongst remaining tests");

                profiled_tests.push(ProfiledTest {
                    test: test_desc_and_fn,
                    result: test.result,
                    exec_time: test.exec_time,
                });
            }
            _ => {}
        }

        Ok(test_runner::Flow::Continue)
    };

    test_runner::run_tests(tests_to_run, on_test_event, test_runner::TestRunStrategy::InProcess(None), false)?;

    Ok(profiled_tests)
}

fn sort_profiled_tests_by_exec_time(profiled_tests: &mut Vec<ProfiledTest>) {
    profiled_tests.sort_by(|a, b| {
        match (a.exec_time, b.exec_time) {
            (Some(exec_time_a), Some(exec_time_b)) => Ord::cmp(&exec_time_a, &exec_time_b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    });
}

fn prioritize_tests_by_distance(tests: &mut Vec<test_runner::Test>, mutations: &'static [&'static MutationMeta]) {
    tests.sort_by(|a, b| {
        let distance_a = mutations.iter().filter_map(|&m| m.reachable_from.get(a.desc.name.as_slice())).reduce(Ord::min);
        let distance_b = mutations.iter().filter_map(|&m| m.reachable_from.get(b.desc.name.as_slice())).reduce(Ord::min);

        match (distance_a, distance_b) {
            (Some(distance_a), Some(distance_b)) => Ord::cmp(distance_a, distance_b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    });
}

fn maximize_mutation_parallelism(tests: &mut Vec<test_runner::Test>, mutations: &'static [&'static MutationMeta]) {
    let mut parallelized_tests = Vec::<test_runner::Test>::with_capacity(tests.len());

    while !tests.is_empty() {
        for mutation in mutations {
            if let Some(test) = tests.iter()
                .position(|t| mutation.reachable_from.contains_key(t.desc.name.as_slice()))
                .map(|i| tests.remove(i))
            {
                parallelized_tests.push(test);
            }
        }
    }

    *tests = parallelized_tests;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum MutationTestResult {
    #[default]
    Undetected,
    Detected,
    TimedOut,
    Crashed,
}

#[derive(Default)]
pub struct MutationTestResults {
    pub result: MutationTestResult,
    pub results_per_test: HashMap<test::TestName, Option<MutationTestResult>>,
}

fn run_tests<S: SubstMap>(mut tests: Vec<test_runner::Test>, mutant: &MutantMeta<S>, exhaustive: bool, thread_pool: Option<ThreadPool>) -> Result<HashMap<u32, MutationTestResults>, Infallible> {
    let mut results = HashMap::<u32, MutationTestResults>::with_capacity(mutant.mutations.len());

    for &mutation in mutant.mutations {
        results.insert(mutation.id, MutationTestResults {
            result: MutationTestResult::Undetected,
            results_per_test: HashMap::with_capacity(mutation.reachable_from.len()),
        });
    }

    tests.retain(|test| mutant.mutations.iter().any(|m| m.reachable_from.contains_key(test.desc.name.as_slice())));
    maximize_mutation_parallelism(&mut tests, mutant.mutations);

    let total_tests_count = tests.len();
    let mut completed_tests_count = 0;

    let on_test_event = |event, remaining_tests: &mut Vec<(test::TestId, test_runner::Test)>| -> Result<_, Infallible> {
        match event {
            test_runner::TestEvent::Result(test) => {
                completed_tests_count += 1;

                let mutation = mutant.mutations.iter().find(|m| m.reachable_from.contains_key(test.desc.name.as_slice()))
                    .expect("only tests which reach mutations should have been run: no mutation is reachable from this test");

                let mutation_results = results.get_mut(&mutation.id).expect("mutation result slot not allocated");

                match test.result {
                    | test_runner::TestResult::Ignored
                    | test_runner::TestResult::Ok => {
                        mutation_results.results_per_test.insert(test.desc.name.clone(), Some(MutationTestResult::Undetected));
                        return Ok(test_runner::Flow::Continue);
                    }

                    | test_runner::TestResult::Failed
                    | test_runner::TestResult::FailedMsg(_) => {
                        mutation_results.results_per_test.insert(test.desc.name.clone(), Some(MutationTestResult::Detected));
                        mutation_results.result = MutationTestResult::Detected;
                    }

                    test_runner::TestResult::CrashedMsg(_) => {
                        mutation_results.results_per_test.insert(test.desc.name.clone(), Some(MutationTestResult::Crashed));
                        // Only mark mutation with crashed verdict if no other test has detected this mutation in a non-crashing way.
                        if mutation_results.result != MutationTestResult::Detected {
                            mutation_results.result = MutationTestResult::Crashed;
                        }
                    }

                    test_runner::TestResult::TimedOut => {
                        mutation_results.results_per_test.insert(test.desc.name.clone(), Some(MutationTestResult::TimedOut));
                        // Only mark mutation with timed-out verdict if no other test has detected this mutation without timing out.
                        if mutation_results.result != MutationTestResult::Detected {
                            mutation_results.result = MutationTestResult::TimedOut;
                        }
                    }
                }

                // By default, tests for a mutation are only run until one of the tests detects the mutation, and
                // test evaluation is stopped early if all mutations are detected.
                if !exhaustive {
                    // Remove any remaining tests from the queue that are for the just detected mutation.
                    remaining_tests.retain(|(_, test)| !mutation.reachable_from.contains_key(test.desc.name.as_slice()));

                    // If all mutations have been detected, stop test evaluation early.
                    if results.iter().all(|(_, mutation_results)| !matches!(mutation_results.result, MutationTestResult::Undetected)) {
                        return Ok(test_runner::Flow::Stop);
                    }
                }
            }
            _ => {}
        }

        Ok(test_runner::Flow::Continue)
    };

    let test_run_strategy = match mutant.is_unsafe() {
        false => test_runner::TestRunStrategy::InProcess(thread_pool),
        true => test_runner::TestRunStrategy::InIsolatedChildProcess({
            let mutant_id = mutant.id;
            Arc::new(move |cmd| {
                cmd.env(MUTEST_ISOLATED_WORKER_MUTANT_ID, mutant_id.to_string());
            })
        }),
    };

    test_runner::run_tests(tests, on_test_event, test_run_strategy, false)?;

    println!("ran {completed} out of {total} {descr}",
        completed = completed_tests_count,
        total = total_tests_count,
        descr = match total_tests_count {
            1 => "test",
            _ => "tests",
        },
    );
    println!();

    Ok(results)
}

#[derive(Clone, Copy, Default)]
pub struct MutationOpStats {
    pub total_mutations_count: usize,
    pub undetected_mutations_count: usize,
    pub timed_out_mutations_count: usize,
    pub crashed_mutations_count: usize,
}

pub struct MutationAnalysisResults {
    pub all_test_runs_failed_successfully: bool,
    pub total_mutations_count: usize,
    pub total_safe_mutations_count: usize,
    pub undetected_mutations_count: usize,
    pub undetected_safe_mutations_count: usize,
    pub timed_out_mutations_count: usize,
    pub timed_out_safe_mutations_count: usize,
    pub crashed_mutations_count: usize,
    pub crashed_safe_mutations_count: usize,
    pub mutation_detection_matrix: MutationDetectionMatrix,
    pub mutation_op_stats: HashMap<&'static str, MutationOpStats>,
    pub duration: Duration,
}

fn run_mutation_analysis<S: SubstMap>(opts: &Options, tests: &[test_runner::Test], mutants: &'static [&'static MutantMeta<S>], active_mutant_handle: &'static ActiveMutantHandle<S>, thread_pool: Option<ThreadPool>) -> MutationAnalysisResults {
    let mut results = MutationAnalysisResults {
        all_test_runs_failed_successfully: true,
        total_mutations_count: 0,
        total_safe_mutations_count: 0,
        undetected_mutations_count: 0,
        undetected_safe_mutations_count: 0,
        timed_out_mutations_count: 0,
        timed_out_safe_mutations_count: 0,
        crashed_mutations_count: 0,
        crashed_safe_mutations_count: 0,
        mutation_detection_matrix: MutationDetectionMatrix::new(mutants.iter().map(|mutant| mutant.mutations.len()).sum()),
        mutation_op_stats: Default::default(),
        duration: Duration::ZERO,
    };

    let t_start = Instant::now();

    for &mutant in mutants {
        // SAFETY: Ideally, since the previous test runs all completed, no other thread is running, no one else is
        //         reading from the handle.
        //         As for lingering test cases from previous test runs, their behaviour will change accordingly, but we
        //         have already marked them as timed out and abandoned them by this point. The behaviour in such cases
        //         stays the same, regardless of whether the handle performs locking or not.
        unsafe { active_mutant_handle.replace(Some(mutant.substitutions.clone())); }

        if opts.verbosity >= 1 {
            print!("{}: ", mutant.id);
        }
        println!("applying mutant with the following mutations:");
        for mutation in mutant.mutations {
            print!("- ");
            if opts.verbosity >= 1 {
                print!("{}: ", mutation.id);
            }
            println!("{unsafe_marker}[{op_name}] {display_name} at {display_location}",
                unsafe_marker = match mutation.safety {
                    MutationSafety::Safe => "",
                    MutationSafety::Tainted => "(tainted) ",
                    MutationSafety::Unsafe => "(unsafe) ",
                },
                op_name = mutation.op_name,
                display_name = mutation.display_name,
                display_location = mutation.display_location,
            );
        }
        println!();

        let mut tests = clone_tests(tests);
        if let config::TestOrdering::MutationDistance = opts.test_ordering {
            prioritize_tests_by_distance(&mut tests, mutant.mutations);
        }

        match run_tests(tests, mutant, opts.exhaustive, thread_pool.clone()) {
            Ok(mut run_results) => {
                for &mutation in mutant.mutations {
                    let op_stats = results.mutation_op_stats.entry(mutation.op_name).or_default();

                    results.total_mutations_count += 1;
                    op_stats.total_mutations_count += 1;
                    if let MutationSafety::Safe = mutation.safety {
                        results.total_safe_mutations_count += 1;
                    }

                    let Some(mutation_result) = run_results.remove(&mutation.id) else { unreachable!() };

                    match mutation_result.result {
                        MutationTestResult::Undetected => {
                            results.all_test_runs_failed_successfully = false;

                            results.undetected_mutations_count += 1;
                            op_stats.undetected_mutations_count += 1;
                            if let MutationSafety::Safe = mutation.safety {
                                results.undetected_safe_mutations_count += 1;
                            }

                            print!("{}", mutation.undetected_diagnostic);
                        }

                        MutationTestResult::Detected => {}
                        MutationTestResult::TimedOut => {
                            results.timed_out_mutations_count += 1;
                            op_stats.timed_out_mutations_count += 1;
                            if let MutationSafety::Safe = mutation.safety {
                                results.timed_out_safe_mutations_count += 1;
                            }

                        }
                        MutationTestResult::Crashed => {
                            results.crashed_mutations_count += 1;
                            op_stats.crashed_mutations_count += 1;
                            if let MutationSafety::Safe = mutation.safety {
                                results.crashed_safe_mutations_count += 1;
                            }
                        }
                    }

                    results.mutation_detection_matrix.insert(mutation.id, mutation_result.result, mutation_result.results_per_test.into_iter());
                }
            }
            Err(_) => { process::exit(ERROR_EXIT_CODE); }
        }
    }

    results.duration = t_start.elapsed();

    results
}

fn print_mutation_analysis_epilogue(results: &MutationAnalysisResults, verbosity: u8) {
    if verbosity >= 1 {
        let mut op_names = results.mutation_op_stats.keys().collect::<Vec<_>>();
        op_names.sort_unstable();

        let op_name_w = op_names.iter().map(|s| s.len()).max().unwrap_or(0);
        let detected_w = results.mutation_op_stats.values().map(|s| (s.total_mutations_count - s.undetected_mutations_count).checked_ilog10().unwrap_or(0) as usize + 1).max().unwrap_or(0);
        let timed_out_w = results.mutation_op_stats.values().map(|s| s.timed_out_mutations_count.checked_ilog10().unwrap_or(0) as usize + 1).max().unwrap_or(0);
        let crashed_w = results.mutation_op_stats.values().map(|s| s.crashed_mutations_count.checked_ilog10().unwrap_or(0) as usize + 1).max().unwrap_or(0);
        let undetected_w = results.mutation_op_stats.values().map(|s| s.undetected_mutations_count.checked_ilog10().unwrap_or(0) as usize + 1).max().unwrap_or(0);

        for op_name in op_names {
            let op_stats = results.mutation_op_stats.get(op_name).map(|s| *s).unwrap_or_default();

            println!("{op_name:>op_name_w$}: {score:>7}. {detected:>detected_w$} detected ({timed_out:>timed_out_w$} timed out; {crashed:>crashed_w$} crashed); {undetected:>undetected_w$} undetected",
                score = format!("{:.2}%",(op_stats.total_mutations_count - op_stats.undetected_mutations_count) as f64 / op_stats.total_mutations_count as f64 * 100_f64),
                detected = op_stats.total_mutations_count - op_stats.undetected_mutations_count,
                timed_out = op_stats.timed_out_mutations_count,
                crashed = op_stats.crashed_mutations_count,
                undetected = op_stats.undetected_mutations_count,
            );
        }

        println!();
    }

    println!("mutations: {score}. {detected} detected ({timed_out} timed out; {crashed} crashed); {undetected} undetected; {total} total",
        score = match results.total_mutations_count {
            0 => "none".to_owned(),
            _ => format!("{:.2}%", (results.total_mutations_count - results.undetected_mutations_count) as f64 / results.total_mutations_count as f64 * 100_f64),
        },
        detected = results.total_mutations_count - results.undetected_mutations_count,
        timed_out = results.timed_out_mutations_count,
        crashed = results.crashed_mutations_count,
        undetected = results.undetected_mutations_count,
        total = results.total_mutations_count,
    );
    println!("     safe: {score}. {detected} detected ({timed_out} timed out; {crashed} crashed); {undetected} undetected; {total} total",
        score = match results.total_safe_mutations_count {
            0 => "none".to_owned(),
            _ => format!("{:.2}%", (results.total_safe_mutations_count - results.undetected_safe_mutations_count) as f64 / results.total_safe_mutations_count as f64 * 100_f64),
        },
        detected = results.total_safe_mutations_count - results.undetected_safe_mutations_count,
        timed_out = results.timed_out_safe_mutations_count,
        crashed = results.crashed_safe_mutations_count,
        undetected = results.undetected_safe_mutations_count,
        total = results.total_safe_mutations_count,
    );
    println!("   unsafe: {score}. {detected} detected ({timed_out} timed out; {crashed} crashed); {undetected} undetected; {total} total",
        score = match results.total_mutations_count - results.total_safe_mutations_count {
            0 => "none".to_owned(),
            _ => format!("{:.2}%", ((results.total_mutations_count - results.total_safe_mutations_count) - (results.undetected_mutations_count - results.undetected_safe_mutations_count)) as f64 / (results.total_mutations_count - results.total_safe_mutations_count) as f64 * 100_f64),
        },
        detected = (results.total_mutations_count - results.total_safe_mutations_count) - (results.undetected_mutations_count - results.undetected_safe_mutations_count),
        timed_out = results.timed_out_mutations_count - results.timed_out_safe_mutations_count,
        crashed = results.crashed_mutations_count - results.crashed_safe_mutations_count,
        undetected = results.undetected_mutations_count - results.undetected_safe_mutations_count,
        total = results.total_mutations_count - results.total_safe_mutations_count,
    );
}

pub fn mutest_main<S: SubstMap>(args: &[&str], tests: Vec<test::TestDescAndFn>, mutants: &'static [&'static MutantMeta<S>], active_mutant_handle: &'static ActiveMutantHandle<S>) {
    let mode = match () {
        _ if let Some(flakes_arg) = args.iter().flat_map(|arg| arg.strip_prefix("--flakes=")).next() => {
            let Some(iterations_count) = flakes_arg.parse::<usize>().ok() else {
                panic!("flaky analysis iterations count must be a valid integer");
            };
            config::Mode::Flakes { iterations_count }
        }

        _ => config::Mode::Evaluate,
    };

    let opts = Options {
        mode,
        verbosity: args.iter().filter(|&arg| *arg == "-v").count() as u8,
        report_timings: args.contains(&"--timings"),
        print_opts: config::PrintOptions {
            detection_matrix: args.contains(&"--print=detection-matrix").then_some(()),
        },
        exhaustive: args.contains(&"--exhaustive"),
        test_timeout: config::TestTimeout::Auto,
        test_ordering: config::TestOrdering::ExecTime,
        use_thread_pool: args.contains(&"--use-thread-pool"),
    };

    let t_start = Instant::now();

    println!("profiling reference test run");
    let t_test_profiling_start = Instant::now();
    let mut profiled_tests = match profile_tests(tests) {
        Ok(tests) => tests,
        Err(_) => { process::exit(ERROR_EXIT_CODE); }
    };
    let test_profiling_duration = t_test_profiling_start.elapsed();

    let failed_profiled_tests = profiled_tests.iter().filter(|test| !matches!(test.result, test_runner::TestResult::Ignored | test_runner::TestResult::Ok)).collect::<Vec<_>>();
    if !failed_profiled_tests.is_empty() {
        for failed_profiled_test in failed_profiled_tests {
            println!("  test {} ... fail", failed_profiled_test.test.desc.name.as_slice());
        }
        println!("not all tests passed, cannot continue");
        process::exit(ERROR_EXIT_CODE);
    }

    sort_profiled_tests_by_exec_time(&mut profiled_tests);

    for profiled_test in &profiled_tests {
        match profiled_test.exec_time {
            Some(exec_time) => println!("{} took {:?}", profiled_test.test.desc.name.as_slice(), exec_time),
            None => println!("{} was not profiled", profiled_test.test.desc.name.as_slice()),
        }
    }
    println!();

    let tests = profiled_tests.into_iter()
        .filter(|profiled_test| !matches!(profiled_test.result, test_runner::TestResult::Ignored))
        .map(|profiled_test| {
            let test::TestDescAndFn { desc, testfn: test_fn } = profiled_test.test;

            let auto_test_timeout = profiled_test.exec_time
                .map(|d| d + Ord::max(d.mul_f32(0.1), Duration::from_secs(1)));

            let timeout = match opts.test_timeout {
                config::TestTimeout::None => None,
                config::TestTimeout::Auto => Some(auto_test_timeout.expect("no test timeout could be deduced automatically")),
                config::TestTimeout::Explicit(test_timeout) => {
                    if let Some(auto_test_timeout) = auto_test_timeout {
                        if test_timeout < auto_test_timeout {
                            println!("warning: explicit test timeout is less than the recommended test timeout based on the profiled reference run\n");
                        }
                    }

                    Some(test_timeout)
                }
            };

            test_runner::Test { desc, test_fn, timeout }
        })
        .collect::<Vec<_>>();

    let thread_pool = opts.use_thread_pool.then(|| {
        let concurrency = test_runner::concurrency();
        ThreadPool::new(concurrency, Some("test_thread_pool".to_owned()), None)
    });
    if let Some(thread_pool) = &thread_pool {
        println!("using thread pool of size {} for running tests", thread_pool.max_thread_count());
        println!();
    }

    match opts.mode {
        config::Mode::Evaluate => {
            let results = run_mutation_analysis(&opts, &tests, mutants, active_mutant_handle, thread_pool);

            if let Some(()) = &opts.print_opts.detection_matrix {
                print_mutation_detection_matrix(&results.mutation_detection_matrix, &tests, !opts.exhaustive);
            }

            print_mutation_analysis_epilogue(&results, opts.verbosity);

            if opts.report_timings {
                println!("\nfinished in {total:.2?} (profiling {profiling:.2?}; tests {tests:.2?})",
                    total = t_start.elapsed(),
                    profiling = test_profiling_duration,
                    tests = results.duration,
                );
            }

            if !results.all_test_runs_failed_successfully {
                process::exit(ERROR_EXIT_CODE);
            }
        }

        config::Mode::Flakes { iterations_count } => {
            let t_flaky_iterations_start = Instant::now();

            let mut results = Vec::with_capacity(iterations_count);

            for iteration in 1..=iterations_count {
                println!("running iteration {iteration} out of {iterations_count}");
                println!();

                let iteration_results = run_mutation_analysis(&opts, &tests, mutants, active_mutant_handle, thread_pool.clone());

                if let Some(()) = &opts.print_opts.detection_matrix {
                    print_mutation_detection_matrix(&iteration_results.mutation_detection_matrix, &tests, !opts.exhaustive);
                }

                print_mutation_analysis_epilogue(&iteration_results, opts.verbosity);

                if opts.report_timings {
                    println!("\nfinished in {tests:.2?}",
                        tests = iteration_results.duration,
                    );
                }

                println!();

                results.push(iteration_results);
            }

            let total_mutations_count = mutants.iter().map(|mutant| mutant.mutations.len()).sum();
            let mutation_detection_matrices = results.iter().map(|run_results| &run_results.mutation_detection_matrix).collect::<Vec<_>>();
            let mutation_flakiness_matrix = MutationFlakinessMatrix::build(total_mutations_count, &mutation_detection_matrices);

            print_mutation_flakiness_matrix(&mutation_flakiness_matrix, &tests);

            print_mutation_flakiness_epilogue(&mutation_flakiness_matrix, &tests);

            println!("\nfinished in {total:.2?} (profiling {profiling:.2?}; iterations {iterations:.2?})",
                total = t_start.elapsed(),
                profiling = test_profiling_duration,
                iterations = t_flaky_iterations_start.elapsed(),
            );
        }
    }
}

const MUTEST_ISOLATED_WORKER_MUTANT_ID: &str = "__MUTEST_ISOLATED_WORKER_MUTANT_ID";

fn mutest_isolated_worker<S: SubstMap>(test: test::TestDescAndFn, mutants: &'static [&'static MutantMeta<S>], active_mutant_handle: &'static ActiveMutantHandle<S>) -> ! {
    let mutant_id = env::var(MUTEST_ISOLATED_WORKER_MUTANT_ID).unwrap()
        .parse::<u32>().expect(&format!("{MUTEST_ISOLATED_WORKER_MUTANT_ID} must be a number"));

    let Some(mutant) = mutants.iter().find(|m| m.id == mutant_id) else {
        panic!("{MUTEST_ISOLATED_WORKER_MUTANT_ID} must be a valid id");
    };

    // SAFETY: No other thread is running yet, no one else is reading from the handle yet.
    unsafe { active_mutant_handle.replace(Some(mutant.substitutions.clone())); }

    test_runner::run_test_in_spawned_subprocess(test);
}

fn mutest_simulate_main<S: SubstMap>(args: &[&str], tests: Vec<test::TestDescAndFn>, mutant: &'static MutantMeta<S>, active_mutant_handle: &'static ActiveMutantHandle<S>) {
    let _verbosity = args.iter().filter(|&arg| *arg == "-v").count() as u8;
    let report_timings = args.contains(&"--timings");
    let use_thread_pool = args.contains(&"--use-thread-pool");

    let t_start = Instant::now();

    let thread_pool = use_thread_pool.then(|| {
        let concurrency = test_runner::concurrency();
        ThreadPool::new(concurrency, Some("test_thread_pool".to_owned()), None)
    });

    print!("running {} tests", tests.len());
    if let Some(thread_pool) = &thread_pool {
        print!(" using thread pool of size {}", thread_pool.max_thread_count());
    }
    println!();

    let total_tests_count = tests.len();
    let mut failed_tests_count = 0;
    let mut ignored_tests_count = 0;

    // SAFETY: No other thread is running yet, no one else is reading from the handle yet.
    unsafe { active_mutant_handle.replace(Some(mutant.substitutions.clone())); }

    let tests_to_run = tests.iter()
        .map(|test| {
            test_runner::Test {
                desc: test.desc.clone(),
                test_fn: make_owned_test_fn(&test.testfn),
                timeout: None,
            }
        })
        .collect::<Vec<_>>();

    let on_test_event = |event, _remaining_tests: &mut Vec<(test::TestId, test_runner::Test)>| -> Result<_, Infallible> {
        match event {
            test_runner::TestEvent::Result(test) => {
                match test.result {
                    test_runner::TestResult::Ignored => {
                        println!("test {} ... \x1b[1;33mignored\x1b[0m", test.desc.name.as_slice());
                        ignored_tests_count += 1;
                    }

                    test_runner::TestResult::Ok => {
                        println!("test {} ... \x1b[1;32mok\x1b[0m", test.desc.name.as_slice());
                    }

                    | test_runner::TestResult::Failed
                    | test_runner::TestResult::FailedMsg(_)
                    | test_runner::TestResult::CrashedMsg(_) => {
                        println!("test {} ... \x1b[1;31mFAILED\x1b[0m", test.desc.name.as_slice());
                        failed_tests_count += 1;
                    }

                    test_runner::TestResult::TimedOut => unreachable!(),
                }
            }
            _ => {}
        }

        Ok(test_runner::Flow::Continue)
    };

    let test_run_strategy = match mutant.is_unsafe() {
        false => test_runner::TestRunStrategy::InProcess(thread_pool),
        true => test_runner::TestRunStrategy::InIsolatedChildProcess({
            let mutant_id = mutant.id;
            Arc::new(move |cmd| {
                cmd.env(MUTEST_ISOLATED_WORKER_MUTANT_ID, mutant_id.to_string());
            })
        }),
    };

    match test_runner::run_tests(tests_to_run, on_test_event, test_run_strategy, false) {
        Ok(_) => {}
        Err(_) => { process::exit(ERROR_EXIT_CODE); }
    }

    println!("test result: {result}. {passed} passed; {failed} failed; {ignored} ignored",
        result = match failed_tests_count {
            0 => "\x1b[1;32mok\x1b[0m",
            _ => "\x1b[1;31mFAILED\x1b[0m",
        },
        passed = total_tests_count - failed_tests_count,
        failed = failed_tests_count,
        ignored = ignored_tests_count,
    );

    if report_timings {
        println!("\nfinished in {total:.2?}",
            total = t_start.elapsed(),
        );
    }

    if failed_tests_count != 0 {
        process::exit(ERROR_EXIT_CODE);
    }
}

pub fn mutest_main_static<S: SubstMap>(tests: &[&test::TestDescAndFn], mutants: &'static [&'static MutantMeta<S>], active_mutant_handle: &'static ActiveMutantHandle<S>) {
    if let Ok(test_name) = env::var(test_runner::TEST_SUBPROCESS_INVOCATION) {
        env::remove_var(test_runner::TEST_SUBPROCESS_INVOCATION);

        let test = tests.iter().find(|test| test.desc.name.as_slice() == test_name)
            .expect(&format!("cannot find test with name `{test_name}`"));
        let test = make_owned_test_def(test);

        mutest_isolated_worker(test, mutants, active_mutant_handle);
    }

    let args = env::args().collect::<Vec<_>>();
    let args = args.iter().map(String::as_ref).collect::<Vec<&str>>();
    let owned_tests = tests.iter().map(|test| make_owned_test_def(test)).collect::<Vec<_>>();

    if let Some(mutation_id) = args.iter().flat_map(|arg| arg.strip_prefix("--simulate=")).next().and_then(|mutation_id| mutation_id.parse::<u32>().ok()) {
        let Some(mutant) = mutants.iter().find(|mutant| mutant.mutations.iter().any(|mutation| mutation.id == mutation_id)) else {
            println!("cannot find mutation with id {mutation_id}");
            process::exit(ERROR_EXIT_CODE);
        };
        if mutant.mutations.len() > 1 {
            println!("cannot simulate mutation: mutation is not in a singleton mutant, disable mutation batching");
            process::exit(ERROR_EXIT_CODE);
        }

        return mutest_simulate_main(&args, owned_tests, mutant, active_mutant_handle);
    }

    mutest_main(&args, owned_tests, mutants, active_mutant_handle)
}
