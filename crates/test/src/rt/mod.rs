//! Internal-only runtime module used for the `wasm_bindgen_test` crate.
//!
//! No API contained in this module will respect semver, these should all be
//! considered private APIs.

// # Architecture of `wasm_bindgen_test`
//
// This module can seem a bit funky, but it's intended to be the runtime support
// of the `#[wasm_bindgen_test]` macro and be amenable to executing Wasm test
// suites. The general idea is that for a Wasm test binary there will be a set
// of functions tagged `#[wasm_bindgen_test]`. It's the job of the runtime
// support to execute all of these functions, collecting and collating the
// results.
//
// This runtime support works in tandem with the `wasm-bindgen-test-runner`
// binary as part of the `wasm-bindgen-cli` package.
//
// ## High Level Overview
//
// Here's a rough and (semi) high level overview of what happens when this crate
// runs.
//
// * First, the user runs `cargo test --target wasm32-unknown-unknown`
//
// * Cargo then compiles all the test suites (aka `tests/*.rs`) as Wasm binaries
//   (the `bin` crate type). These binaries all have entry points that are
//   `main` functions, but it's actually not used. The binaries are also
//   compiled with `--test`, which means they're linked to the standard `test`
//   crate, but this crate doesn't work on Wasm and so we bypass it entirely.
//
// * Instead of using `#[test]`, which doesn't work, users wrote tests with
//   `#[wasm_bindgen_test]`. This macro expands to a bunch of `#[no_mangle]`
//   functions with known names (currently named `__wbg_test_*`).
//
// * Next up, Cargo was configured via its test runner support to execute the
//   `wasm-bindgen-test-runner` binary. Instead of what Cargo normally does,
//   executing `target/wasm32-unknown-unknown/debug/deps/foo-xxxxx.wasm` (which
//   will fail as we can't actually execute was binaries), Cargo will execute
//   `wasm-bindgen-test-runner target/.../foo-xxxxx.wasm`.
//
// * The `wasm-bindgen-test-runner` binary takes over. It runs `wasm-bindgen`
//   over the binary, generating JS bindings and such. It also figures out if
//   we're running in node.js or a browser.
//
// * The `wasm-bindgen-test-runner` binary generates a JS entry point. This
//   entry point creates a `Context` below. The runner binary also parses the
//   Wasm file and finds all functions that are named `__wbg_test_*`. The
//   generate file gathers up all these functions into an array and then passes
//   them to `Context` below. Note that these functions are passed as *JS
//   values*.
//
// * Somehow, the runner then executes the JS file. This may be with node.js, it
//   may serve up files in a server and wait for the user, or it serves up files
//   in a server and starts headless testing.
//
// * Testing starts, it loads all the modules using either ES imports or Node
//   `require` statements. Everything is loaded in JS now.
//
// * A `Context` is created. The `Context` is forwarded the CLI arguments of the
//   original `wasm-bindgen-test-runner` in an environment specific fashion.
//   This is used for test filters today.
//
// * The `Context::run` function is called. Again, the generated JS has gathered
//   all Wasm tests to be executed into a list, and it's passed in here.
//
// * Next, `Context::run` returns a `Promise` representing the eventual
//   execution of all the tests. The Rust `Future` that's returned will work
//   with the tests to ensure that everything's executed by the time the
//   `Promise` resolves.
//
// * When a test executes, it's executing an entry point generated by
//   `#[wasm_bindgen_test]`. The test informs the `Context` of its name and
//   other metadata, and then `Context::execute_*` function creates a future
//   representing the execution of the test. This feeds back into the future
//   returned by `Context::run` to finish the test suite.
//
// * Finally, after all tests are run, the `Context`'s future resolves, prints
//   out all the result, and finishes in JS.
//
// ## Other various notes
//
// Phew, that was a lot! Some other various bits and pieces you may want to be
// aware of are throughout the code. These include things like how printing
// results is different in node vs a browser, or how we even detect if we're in
// node or a browser.
//
// Overall this is all somewhat in flux as it's pretty new, and feedback is
// always of course welcome!

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::{self, Display};
use core::future::Future;
use core::pin::Pin;
use core::task::{self, Poll};
use js_sys::{Array, Function, Promise};
pub use wasm_bindgen;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

// Maximum number of tests to execute concurrently. Eventually this should be a
// configuration option specified at runtime or at compile time rather than
// baked in here.
//
// Currently the default is 1 because the DOM has a lot of shared state, and
// conccurrently doing things by default would likely end up in a bad situation.
const CONCURRENCY: usize = 1;

pub mod browser;
pub mod detect;
pub mod node;
mod scoped_tls;
pub mod worker;

/// Runtime test harness support instantiated in JS.
///
/// The node.js entry script instantiates a `Context` here which is used to
/// drive test execution.
#[wasm_bindgen(js_name = WasmBindgenTestContext)]
pub struct Context {
    state: Rc<State>,
}

struct State {
    /// An optional filter used to restrict which tests are actually executed
    /// and which are ignored. This is passed via the `args` function which
    /// comes from the command line of `wasm-bindgen-test-runner`. Currently
    /// this is the only "CLI option"
    filter: RefCell<Option<String>>,

    /// Include ignored tests.
    include_ignored: Cell<bool>,

    /// Tests to skip.
    skip: RefCell<Vec<String>>,

    /// Counter of the number of tests that have succeeded.
    succeeded: Cell<usize>,

    /// Counter of the number of tests that have been filtered
    filtered: Cell<usize>,

    /// Counter of the number of tests that have been ignored
    ignored: Cell<usize>,

    /// A list of all tests which have failed.
    ///
    /// Each test listed here is paired with a `JsValue` that represents the
    /// exception thrown which caused the test to fail.
    failures: RefCell<Vec<(Test, Failure)>>,

    /// Remaining tests to execute, when empty we're just waiting on the
    /// `Running` tests to finish.
    remaining: RefCell<Vec<Test>>,

    /// List of currently executing tests. These tests all involve some level
    /// of asynchronous work, so they're sitting on the running list.
    running: RefCell<Vec<Test>>,

    /// How to actually format output, either node.js or browser-specific
    /// implementation.
    formatter: Box<dyn Formatter>,

    /// Timing the total duration.
    timer: Option<Timer>,
}

/// Failure reasons.
enum Failure {
    /// Normal failing test.
    Error(JsValue),
    /// A test that `should_panic` but didn't.
    ShouldPanic,
    /// A test that `should_panic` with a specific message,
    /// but panicked with a different message.
    ShouldPanicExpected,
}

/// Representation of one test that needs to be executed.
///
/// Tests are all represented as futures, and tests perform no work until their
/// future is polled.
struct Test {
    name: String,
    future: Pin<Box<dyn Future<Output = Result<(), JsValue>>>>,
    output: Rc<RefCell<Output>>,
    should_panic: Option<Option<&'static str>>,
}

/// Captured output of each test.
#[derive(Default)]
struct Output {
    debug: String,
    log: String,
    info: String,
    warn: String,
    error: String,
    panic: String,
    should_panic: bool,
}

enum TestResult {
    Ok,
    Err(JsValue),
    Ignored(Option<String>),
}

impl From<Result<(), JsValue>> for TestResult {
    fn from(value: Result<(), JsValue>) -> Self {
        match value {
            Ok(()) => Self::Ok,
            Err(err) => Self::Err(err),
        }
    }
}

impl Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestResult::Ok => write!(f, "ok"),
            TestResult::Err(_) => write!(f, "FAIL"),
            TestResult::Ignored(None) => write!(f, "ignored"),
            TestResult::Ignored(Some(reason)) => write!(f, "ignored, {}", reason),
        }
    }
}

trait Formatter {
    /// Writes a line of output, typically status information.
    fn writeln(&self, line: &str);

    /// Log the result of a test, either passing or failing.
    fn log_test(&self, name: &str, result: &TestResult);

    /// Convert a thrown value into a string, using platform-specific apis
    /// perhaps to turn the error into a string.
    fn stringify_error(&self, val: &JsValue) -> String;
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    #[doc(hidden)]
    pub fn js_console_log(s: &str);

    #[wasm_bindgen(js_namespace = console, js_name = error)]
    #[doc(hidden)]
    pub fn js_console_error(s: &str);

    // General-purpose conversion into a `String`.
    #[wasm_bindgen(js_name = String)]
    fn stringify(val: &JsValue) -> String;

    type Global;

    #[wasm_bindgen(method, getter)]
    fn performance(this: &Global) -> JsValue;

    /// Type for the [`Performance` object](https://developer.mozilla.org/en-US/docs/Web/API/Performance).
    type Performance;

    /// Binding to [`Performance.now()`](https://developer.mozilla.org/en-US/docs/Web/API/Performance/now).
    #[wasm_bindgen(method)]
    fn now(this: &Performance) -> f64;
}

/// Internal implementation detail of the `console_log!` macro.
pub fn console_log(args: &fmt::Arguments) {
    js_console_log(&args.to_string());
}

/// Internal implementation detail of the `console_error!` macro.
pub fn console_error(args: &fmt::Arguments) {
    js_console_error(&args.to_string());
}

#[wasm_bindgen(js_class = WasmBindgenTestContext)]
impl Context {
    /// Creates a new context ready to run tests.
    ///
    /// A `Context` is the main structure through which test execution is
    /// coordinated, and this will collect output and results for all executed
    /// tests.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Context {
        fn panic_handling(mut message: String) {
            let should_panic = CURRENT_OUTPUT.with(|output| {
                let mut output = output.borrow_mut();
                output.panic.push_str(&message);
                output.should_panic
            });

            // See https://github.com/rustwasm/console_error_panic_hook/blob/4dc30a5448ed3ffcfb961b1ad54d000cca881b84/src/lib.rs#L83-L123.
            if !should_panic {
                #[wasm_bindgen]
                extern "C" {
                    type Error;

                    #[wasm_bindgen(constructor)]
                    fn new() -> Error;

                    #[wasm_bindgen(method, getter)]
                    fn stack(error: &Error) -> String;
                }

                message.push_str("\n\nStack:\n\n");
                let e = Error::new();
                let stack = e.stack();
                message.push_str(&stack);

                message.push_str("\n\n");

                js_console_error(&message);
            }
        }
        #[cfg(feature = "std")]
        static SET_HOOK: std::sync::Once = std::sync::Once::new();
        #[cfg(feature = "std")]
        SET_HOOK.call_once(|| {
            std::panic::set_hook(Box::new(|panic_info| {
                panic_handling(panic_info.to_string());
            }));
        });
        #[cfg(all(
            not(feature = "std"),
            target_arch = "wasm32",
            any(target_os = "unknown", target_os = "none")
        ))]
        #[panic_handler]
        fn panic_handler(panic_info: &core::panic::PanicInfo<'_>) -> ! {
            panic_handling(panic_info.to_string());
            core::arch::wasm32::unreachable();
        }

        let formatter = match detect::detect() {
            detect::Runtime::Browser => Box::new(browser::Browser::new()) as Box<dyn Formatter>,
            detect::Runtime::Node => Box::new(node::Node::new()) as Box<dyn Formatter>,
            detect::Runtime::Worker => Box::new(worker::Worker::new()) as Box<dyn Formatter>,
        };

        let timer = Timer::new();

        Context {
            state: Rc::new(State {
                filter: Default::default(),
                include_ignored: Default::default(),
                skip: Default::default(),
                failures: Default::default(),
                filtered: Default::default(),
                ignored: Default::default(),
                remaining: Default::default(),
                running: Default::default(),
                succeeded: Default::default(),
                formatter,
                timer,
            }),
        }
    }

    /// Handle `--include-ignored` flag.
    pub fn include_ignored(&mut self, include_ignored: bool) {
        self.state.include_ignored.set(include_ignored);
    }

    /// Handle `--skip` arguments.
    pub fn skip(&mut self, skip: Vec<String>) {
        *self.state.skip.borrow_mut() = skip;
    }

    /// Handle filter argument.
    pub fn filter(&mut self, filter: Option<String>) {
        *self.state.filter.borrow_mut() = filter;
    }

    /// Executes a list of tests, returning a promise representing their
    /// eventual completion.
    ///
    /// This is the main entry point for executing tests. All the tests passed
    /// in are the JS `Function` object that was plucked off the
    /// `WebAssembly.Instance` exports list.
    ///
    /// The promise returned resolves to either `true` if all tests passed or
    /// `false` if at least one test failed.
    pub fn run(&self, tests: Vec<JsValue>) -> Promise {
        let noun = if tests.len() == 1 { "test" } else { "tests" };
        self.state
            .formatter
            .writeln(&format!("running {} {}", tests.len(), noun));

        // Execute all our test functions through their Wasm shims (unclear how
        // to pass native function pointers around here). Each test will
        // execute one of the `execute_*` tests below which will push a
        // future onto our `remaining` list, which we'll process later.
        let cx_arg = (self as *const Context as u32).into();
        for test in tests {
            match Function::from(test).call1(&JsValue::null(), &cx_arg) {
                Ok(_) => {}
                Err(e) => {
                    panic!(
                        "exception thrown while creating a test: {}",
                        self.state.formatter.stringify_error(&e)
                    );
                }
            }
        }

        // Now that we've collected all our tests we wrap everything up in a
        // future to actually do all the processing, and pass it out to JS as a
        // `Promise`.
        let state = self.state.clone();
        future_to_promise(async {
            let passed = ExecuteTests(state).await;
            Ok(JsValue::from(passed))
        })
    }
}

crate::scoped_thread_local!(static CURRENT_OUTPUT: RefCell<Output>);

/// Handler for `console.log` invocations.
///
/// If a test is currently running it takes the `args` array and stringifies
/// it and appends it to the current output of the test. Otherwise it passes
/// the arguments to the original `console.log` function, psased as
/// `original`.
//
// TODO: how worth is it to actually capture the output here? Due to the nature
// of futures/js we can't guarantee that all output is captured because JS code
// could just be executing in the void and we wouldn't know which test to
// attach it to. The main `test` crate in the rust repo also has issues about
// how not all output is captured, causing some inconsistencies sometimes.
#[wasm_bindgen]
pub fn __wbgtest_console_log(args: &Array) {
    record(args, |output| &mut output.log)
}

/// Handler for `console.debug` invocations. See above.
#[wasm_bindgen]
pub fn __wbgtest_console_debug(args: &Array) {
    record(args, |output| &mut output.debug)
}

/// Handler for `console.info` invocations. See above.
#[wasm_bindgen]
pub fn __wbgtest_console_info(args: &Array) {
    record(args, |output| &mut output.info)
}

/// Handler for `console.warn` invocations. See above.
#[wasm_bindgen]
pub fn __wbgtest_console_warn(args: &Array) {
    record(args, |output| &mut output.warn)
}

/// Handler for `console.error` invocations. See above.
#[wasm_bindgen]
pub fn __wbgtest_console_error(args: &Array) {
    record(args, |output| &mut output.error)
}

fn record(args: &Array, dst: impl FnOnce(&mut Output) -> &mut String) {
    if !CURRENT_OUTPUT.is_set() {
        return;
    }

    CURRENT_OUTPUT.with(|output| {
        let mut out = output.borrow_mut();
        let dst = dst(&mut out);
        args.for_each(&mut |val, idx, _array| {
            if idx != 0 {
                dst.push(' ');
            }
            dst.push_str(&stringify(&val));
        });
        dst.push('\n');
    });
}

/// Similar to [`std::process::Termination`], but for wasm-bindgen tests.
pub trait Termination {
    /// Convert this into a JS result.
    fn into_js_result(self) -> Result<(), JsValue>;
}

impl Termination for () {
    fn into_js_result(self) -> Result<(), JsValue> {
        Ok(())
    }
}

impl<E: core::fmt::Debug> Termination for Result<(), E> {
    fn into_js_result(self) -> Result<(), JsValue> {
        self.map_err(|e| JsError::new(&format!("{:?}", e)).into())
    }
}

impl Context {
    /// Entry point for a synchronous test in wasm. The `#[wasm_bindgen_test]`
    /// macro generates invocations of this method.
    pub fn execute_sync<T: Termination>(
        &self,
        name: &str,
        f: impl 'static + FnOnce() -> T,
        should_panic: Option<Option<&'static str>>,
        ignore: Option<Option<&'static str>>,
    ) {
        self.execute(name, async { f().into_js_result() }, should_panic, ignore);
    }

    /// Entry point for an asynchronous in wasm. The
    /// `#[wasm_bindgen_test(async)]` macro generates invocations of this
    /// method.
    pub fn execute_async<F>(
        &self,
        name: &str,
        f: impl FnOnce() -> F + 'static,
        should_panic: Option<Option<&'static str>>,
        ignore: Option<Option<&'static str>>,
    ) where
        F: Future + 'static,
        F::Output: Termination,
    {
        self.execute(
            name,
            async { f().await.into_js_result() },
            should_panic,
            ignore,
        )
    }

    fn execute(
        &self,
        name: &str,
        test: impl Future<Output = Result<(), JsValue>> + 'static,
        should_panic: Option<Option<&'static str>>,
        ignore: Option<Option<&'static str>>,
    ) {
        // Split away
        let name = name.split_once("::").unwrap().1;
        // If our test is filtered out, record that it was filtered and move
        // on, nothing to do here.
        let filter = self.state.filter.borrow();
        if let Some(filter) = &*filter {
            if !name.contains(filter) {
                let filtered = self.state.filtered.get();
                self.state.filtered.set(filtered + 1);
                return;
            }
        }

        for skip in &*self.state.skip.borrow() {
            if name.contains(skip) {
                let filtered = self.state.filtered.get();
                self.state.filtered.set(filtered + 1);
                return;
            }
        }

        if !self.state.include_ignored.get() {
            if let Some(ignore) = ignore {
                self.state
                    .formatter
                    .log_test(name, &TestResult::Ignored(ignore.map(str::to_owned)));
                let ignored = self.state.ignored.get();
                self.state.ignored.set(ignored + 1);
                return;
            }
        }

        // Looks like we've got a test that needs to be executed! Push it onto
        // the list of remaining tests.
        let output = Output {
            should_panic: should_panic.is_some(),
            ..Default::default()
        };
        let output = Rc::new(RefCell::new(output));
        let future = TestFuture {
            output: output.clone(),
            test,
        };
        self.state.remaining.borrow_mut().push(Test {
            name: name.to_string(),
            future: Pin::from(Box::new(future)),
            output,
            should_panic,
        });
    }
}

struct ExecuteTests(Rc<State>);

impl Future for ExecuteTests {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<bool> {
        let mut running = self.0.running.borrow_mut();
        let mut remaining = self.0.remaining.borrow_mut();

        // First up, try to make progress on all active tests. Remove any
        // finished tests.
        for i in (0..running.len()).rev() {
            let result = match running[i].future.as_mut().poll(cx) {
                Poll::Ready(result) => result,
                Poll::Pending => continue,
            };
            let test = running.remove(i);
            self.0.log_test_result(test, result.into());
        }

        // Next up, try to schedule as many tests as we can. Once we get a test
        // we `poll` it once to ensure we'll receive notifications. We only
        // want to schedule up to a maximum amount of work though, so this may
        // not schedule all tests.
        while running.len() < CONCURRENCY {
            let mut test = match remaining.pop() {
                Some(test) => test,
                None => break,
            };
            let result = match test.future.as_mut().poll(cx) {
                Poll::Ready(result) => result,
                Poll::Pending => {
                    running.push(test);
                    continue;
                }
            };
            self.0.log_test_result(test, result.into());
        }

        // Tests are still executing, we're registered to get a notification,
        // keep going.
        if running.len() != 0 {
            return Poll::Pending;
        }

        // If there are no tests running then we must have finished everything,
        // so we shouldn't have any more remaining tests either.
        assert_eq!(remaining.len(), 0);

        self.0.print_results();
        let all_passed = self.0.failures.borrow().len() == 0;
        Poll::Ready(all_passed)
    }
}

impl State {
    fn log_test_result(&self, test: Test, result: TestResult) {
        // Save off the test for later processing when we print the final
        // results.
        if let Some(should_panic) = test.should_panic {
            if let TestResult::Err(_e) = result {
                if let Some(expected) = should_panic {
                    if !test.output.borrow().panic.contains(expected) {
                        self.formatter
                            .log_test(&test.name, &TestResult::Err(JsValue::NULL));
                        self.failures
                            .borrow_mut()
                            .push((test, Failure::ShouldPanicExpected));
                        return;
                    }
                }

                self.formatter.log_test(&test.name, &TestResult::Ok);
                self.succeeded.set(self.succeeded.get() + 1);
            } else {
                self.formatter
                    .log_test(&test.name, &TestResult::Err(JsValue::NULL));
                self.failures
                    .borrow_mut()
                    .push((test, Failure::ShouldPanic));
            }
        } else {
            self.formatter.log_test(&test.name, &result);

            match result {
                TestResult::Ok => self.succeeded.set(self.succeeded.get() + 1),
                TestResult::Err(e) => self.failures.borrow_mut().push((test, Failure::Error(e))),
                _ => (),
            }
        }
    }

    fn print_results(&self) {
        let failures = self.failures.borrow();
        if failures.len() > 0 {
            self.formatter.writeln("\nfailures:\n");
            for (test, failure) in failures.iter() {
                self.print_failure(test, failure);
            }
            self.formatter.writeln("failures:\n");
            for (test, _) in failures.iter() {
                self.formatter.writeln(&format!("    {}", test.name));
            }
        }
        let finished_in = if let Some(timer) = &self.timer {
            format!("; finished in {:.2?}s", timer.elapsed())
        } else {
            String::new()
        };
        self.formatter.writeln("");
        self.formatter.writeln(&format!(
            "test result: {}. \
             {} passed; \
             {} failed; \
             {} ignored; \
             {} filtered out\
             {}\n",
            if failures.len() == 0 { "ok" } else { "FAILED" },
            self.succeeded.get(),
            failures.len(),
            self.ignored.get(),
            self.filtered.get(),
            finished_in,
        ));
    }

    fn accumulate_console_output(&self, logs: &mut String, which: &str, output: &str) {
        if output.is_empty() {
            return;
        }
        logs.push_str(which);
        logs.push_str(" output:\n");
        logs.push_str(&tab(output));
        logs.push('\n');
    }

    fn print_failure(&self, test: &Test, failure: &Failure) {
        let mut logs = String::new();
        let output = test.output.borrow();

        match failure {
            Failure::ShouldPanic => {
                logs.push_str(&format!(
                    "note: {} did not panic as expected\n\n",
                    test.name
                ));
            }
            Failure::ShouldPanicExpected => {
                logs.push_str("note: panic did not contain expected string\n");
                logs.push_str(&format!("      panic message: `\"{}\"`,\n", output.panic));
                logs.push_str(&format!(
                    " expected substring: `\"{}\"`\n\n",
                    test.should_panic.unwrap().unwrap()
                ));
            }
            _ => (),
        }

        self.accumulate_console_output(&mut logs, "debug", &output.debug);
        self.accumulate_console_output(&mut logs, "log", &output.log);
        self.accumulate_console_output(&mut logs, "info", &output.info);
        self.accumulate_console_output(&mut logs, "warn", &output.warn);
        self.accumulate_console_output(&mut logs, "error", &output.error);

        if let Failure::Error(error) = failure {
            logs.push_str("JS exception that was thrown:\n");
            let error_string = self.formatter.stringify_error(error);
            logs.push_str(&tab(&error_string));
        }

        let msg = format!("---- {} output ----\n{}", test.name, tab(&logs));
        self.formatter.writeln(&msg);
    }
}

/// A wrapper future around each test
///
/// This future is what's actually executed for each test and is what's stored
/// inside of a `Test`. This wrapper future performs two critical functions:
///
/// * First, every time when polled, it configures the `CURRENT_OUTPUT` tls
///   variable to capture output for the current test. That way at least when
///   we've got Rust code running we'll be able to capture output.
///
/// * Next, this "catches panics". Right now all Wasm code is configured as
///   panic=abort, but it's more like an exception in JS. It's pretty sketchy
///   to actually continue executing Rust code after an "abort", but we don't
///   have much of a choice for now.
///
///   Panics are caught here by using a shim function that is annotated with
///   `catch` so we can capture JS exceptions (which Rust panics become). This
///   way if any Rust code along the execution of a test panics we'll hopefully
///   capture it.
///
/// Note that both of the above aspects of this future are really just best
/// effort. This is all a bit of a hack right now when it comes down to it and
/// it definitely won't work in some situations. Hopefully as those situations
/// arise though we can handle them!
///
/// The good news is that everything should work flawlessly in the case where
/// tests have no output and execute successfully. And everyone always writes
/// perfect code on the first try, right? *sobs*
struct TestFuture<F> {
    output: Rc<RefCell<Output>>,
    test: F,
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch)]
    fn __wbg_test_invoke(f: &mut dyn FnMut()) -> Result<(), JsValue>;
}

impl<F: Future<Output = Result<(), JsValue>>> Future for TestFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Self::Output> {
        let output = self.output.clone();
        // Use `new_unchecked` here to project our own pin, and we never
        // move `test` so this should be safe
        let test = unsafe { Pin::map_unchecked_mut(self, |me| &mut me.test) };
        let mut future_output = None;
        let result = CURRENT_OUTPUT.set(&output, || {
            let mut test = Some(test);
            __wbg_test_invoke(&mut || {
                let test = test.take().unwrap_throw();
                future_output = Some(test.poll(cx))
            })
        });
        match (result, future_output) {
            (_, Some(Poll::Ready(result))) => Poll::Ready(result),
            (_, Some(Poll::Pending)) => Poll::Pending,
            (Err(e), _) => Poll::Ready(Err(e)),
            (Ok(_), None) => wasm_bindgen::throw_str("invalid poll state"),
        }
    }
}

fn tab(s: &str) -> String {
    let mut result = String::new();
    for line in s.lines() {
        result.push_str("    ");
        result.push_str(line);
        result.push('\n');
    }
    result
}

struct Timer {
    performance: Performance,
    started: f64,
}

impl Timer {
    fn new() -> Option<Self> {
        let global: Global = js_sys::global().unchecked_into();
        let performance = global.performance();
        (!performance.is_undefined()).then(|| {
            let performance: Performance = performance.unchecked_into();
            let started = performance.now();
            Self {
                performance,
                started,
            }
        })
    }

    fn elapsed(&self) -> f64 {
        (self.performance.now() - self.started) / 1000.
    }
}
