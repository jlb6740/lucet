use crate::modules::{compile_hello, fib_mock, null_mock};
use criterion::Criterion;
use lucet_runtime::{DlModule, InstanceHandle, Limits, Module, Region, RegionCreate};
use lucetc::OptLevel;
use rayon::prelude::*;
use std::sync::Arc;
use tempfile::TempDir;

/// Common definiton of OptLevel
const BENCHMARK_OPT_LEVEL: OptLevel = OptLevel::SpeedAndSize;

/// Parallel instantiation.
///
/// This measures how well the region handles concurrent instantiations from multiple
/// threads. Scaling is not necessarily the point here, due to the locks on the region freelist and
/// memory management syscalls, but we do want to make sure the concurrent case isn't slower than
/// single-threaded.
fn par_instantiate<R: RegionCreate + 'static>(c: &mut Criterion) {
    const INSTANCES_PER_RUN: usize = 2000;

    fn setup<R: RegionCreate + 'static>() -> (Arc<R>, Vec<Option<InstanceHandle>>) {
        let region = R::create(INSTANCES_PER_RUN, &Limits::default()).unwrap();
        let mut handles = vec![];
        handles.resize_with(INSTANCES_PER_RUN, || None);
        (region, handles)
    }

    fn body<R: Region>(
        num_threads: usize,
        module: Arc<dyn Module>,
        region: Arc<R>,
        handles: &mut [Option<InstanceHandle>],
    ) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap()
            .install(|| {
                handles
                    .par_iter_mut()
                    .for_each(|handle| *handle = Some(region.new_instance(module.clone()).unwrap()))
            })
    }

    let workdir = TempDir::new().expect("create working directory");

    let so_file = workdir.path().join("out.so");
    compile_hello(&so_file, BENCHMARK_OPT_LEVEL);

    let module = DlModule::load(&so_file).unwrap();

    let bench = criterion::ParameterizedBenchmark::new(
        format!("par_instantiate ({})", R::TYPE_NAME),
        move |b, &num_threads| {
            b.iter_batched(
                setup,
                |(region, mut handles): (Arc<R>, _)| {
                    body(num_threads, module.clone(), region, handles.as_mut_slice())
                },
                criterion::BatchSize::SmallInput,
            )
        },
        (1..=num_cpus::get_physical()).collect::<Vec<usize>>(),
    )
    .sample_size(10);

    c.bench("par", bench);

    workdir.close().unwrap();
}

/// Run a function in parallel.
fn par_run<R: RegionCreate + 'static>(
    name: &str,
    instances_per_run: usize,
    module: Arc<dyn Module>,
    c: &mut Criterion,
) {
    let setup = move || {
        let region = R::create(instances_per_run, &Limits::default()).unwrap();

        (0..instances_per_run)
            .map(|_| region.new_instance(module.clone()).unwrap())
            .collect::<Vec<InstanceHandle>>()
    };

    fn body(num_threads: usize, handles: &mut [InstanceHandle]) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap()
            .install(|| {
                handles.par_iter_mut().for_each(|handle| {
                    handle.run("f", &[]).unwrap();
                })
            })
    }

    let bench = criterion::ParameterizedBenchmark::new(
        name,
        move |b, &num_threads| {
            b.iter_batched_ref(
                setup.clone(),
                |handles| body(num_threads, handles.as_mut_slice()),
                criterion::BatchSize::SmallInput,
            )
        },
        (1..=num_cpus::get_physical()).collect::<Vec<usize>>(),
    )
    .sample_size(10);

    c.bench("par", bench);
}

/// Run a function in parallel, controlling signal handlers and stacks manually.
fn par_run_manual_signals<R: RegionCreate + 'static>(
    name: &str,
    instances_per_run: usize,
    module: Arc<dyn Module>,
    c: &mut Criterion,
) {
    struct SignalState {
        #[allow(unused)]
        // this has to stick around so the sigstack remains valid memory
        sigstack: Vec<u8>,
        saved_sigstack: libc::stack_t,
    }

    impl SignalState {
        fn new() -> Self {
            lucet_runtime::install_lucet_signal_handler();

            let mut sigstack = vec![0; lucet_runtime::DEFAULT_SIGNAL_STACK_SIZE];
            let sigstack_raw = libc::stack_t {
                ss_sp: sigstack.as_mut_ptr() as *mut _,
                ss_flags: 0,
                ss_size: lucet_runtime::DEFAULT_SIGNAL_STACK_SIZE,
            };
            let mut saved_sigstack = std::mem::MaybeUninit::<libc::stack_t>::uninit();
            let saved_sigstack = unsafe {
                libc::sigaltstack(&sigstack_raw, saved_sigstack.as_mut_ptr());
                saved_sigstack.assume_init()
            };

            Self {
                sigstack,
                saved_sigstack,
            }
        }
    }

    impl Drop for SignalState {
        fn drop(&mut self) {
            lucet_runtime::remove_lucet_signal_handler();
            unsafe {
                libc::sigaltstack(&self.saved_sigstack, std::ptr::null_mut());
            }
        }
    }

    let setup = move || {
        let region = R::create(instances_per_run, &Limits::default()).unwrap();

        (
            (0..instances_per_run)
                .map(|_| {
                    let mut inst = region.new_instance(module.clone()).unwrap();
                    inst.ensure_signal_handler_installed(false);
                    inst.ensure_sigstack_installed(false);
                    inst
                })
                .collect::<Vec<InstanceHandle>>(),
            SignalState::new(),
        )
    };

    fn body(num_threads: usize, handles: &mut [InstanceHandle]) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap()
            .install(|| {
                handles.par_iter_mut().for_each(|handle| {
                    handle.run("f", &[]).unwrap();
                })
            })
    }

    let bench = criterion::ParameterizedBenchmark::new(
        name,
        move |b, &num_threads| {
            b.iter_batched_ref(
                setup.clone(),
                |(handles, _)| body(num_threads, handles.as_mut_slice()),
                criterion::BatchSize::SmallInput,
            )
        },
        (1..=num_cpus::get_physical()).collect::<Vec<usize>>(),
    )
    .sample_size(10);

    c.bench("par", bench);
}

/// Run a trivial function in parallel.
///
/// This measures how well the region handles concurrent executions from multiple threads. Since the
/// body of the function is empty, scaling is not necessarily the point here, rather we want to make
/// sure that the locks for signal handling don't unduly slow the program down with multiple
/// threads.
fn par_run_null<R: RegionCreate + 'static>(c: &mut Criterion) {
    par_run::<R>(
        &format!("par_run_null ({})", R::TYPE_NAME),
        1000,
        null_mock(),
        c,
    );
    par_run_manual_signals::<R>(
        &format!("par_run_null_manual_signals ({})", R::TYPE_NAME),
        1000,
        null_mock(),
        c,
    );
}

/// Run a computation-heavy function in parallel.
///
/// Since running multiple independent fibonaccis is embarassingly parallel, this should scale close
/// to linearly.
fn par_run_fib<R: RegionCreate + 'static>(c: &mut Criterion) {
    par_run::<R>(
        &format!("par_run_fib ({})", R::TYPE_NAME),
        1000,
        fib_mock(),
        c,
    );
    par_run_manual_signals::<R>(
        &format!("par_run_fib_manual_signals ({})", R::TYPE_NAME),
        1000,
        fib_mock(),
        c,
    );
}

pub fn par_benches<R: RegionCreate + 'static>(c: &mut Criterion) {
    par_instantiate::<R>(c);
    par_run_null::<R>(c);
    par_run_fib::<R>(c);
}
