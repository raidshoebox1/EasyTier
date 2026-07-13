use std::ffi::CString;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use jni::JNIEnv;
use jni::objects::{GlobalRef, JObject, JValue};
use once_cell::sync::Lazy;

// Android log priorities (from android/log.h)
const ANDROID_LOG_VERBOSE: i32 = 2;
const ANDROID_LOG_DEBUG: i32 = 3;
const ANDROID_LOG_INFO: i32 = 4;
const ANDROID_LOG_WARN: i32 = 5;
const ANDROID_LOG_ERROR: i32 = 6;

extern "C" {
    fn __android_log_write(prio: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
}

// Maximum log level stored as u8 (matches log::LevelFilter discriminants).
// Default: 4 = Debug (matches the previous android_logger::init_once setting).
static MAX_LEVEL: AtomicU8 = AtomicU8::new(4);

// ── Java callback for log forwarding ──

struct JniLogCallback {
    java_vm: jni::JavaVM,
    callback: GlobalRef,
}

static LOG_CALLBACK: Lazy<Mutex<Option<Arc<JniLogCallback>>>> = Lazy::new(|| Mutex::new(None));

impl JniLogCallback {
    fn on_log(&self, level: log::Level, target: &str, message: &str) -> Result<(), String> {
        let mut env = self
            .java_vm
            .attach_current_thread()
            .map_err(|e| format!("Failed to attach thread: {:?}", e))?;

        let level_str = match level {
            log::Level::Error => "E",
            log::Level::Warn => "W",
            log::Level::Info => "I",
            log::Level::Debug => "D",
            log::Level::Trace => "T",
        };

        let j_level = env
            .new_string(level_str)
            .map_err(|e| format!("Failed to create level string: {:?}", e))?;
        let j_target = env
            .new_string(target)
            .map_err(|e| format!("Failed to create target string: {:?}", e))?;
        let j_message = env
            .new_string(message)
            .map_err(|e| format!("Failed to create message string: {:?}", e))?;

        if let Err(e) = env.call_method(
            self.callback.as_obj(),
            "onLog",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[
                JValue::from(&j_level),
                JValue::from(&j_target),
                JValue::from(&j_message),
            ],
        ) {
            // Clear pending exception to avoid corrupting JNI state
            let _ = env.exception_clear();
            return Err(format!("Failed to call onLog: {:?}", e));
        }

        Ok(())
    }
}

// ── Custom logger ──

struct CallbackLogger;

impl log::Log for CallbackLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let max = log::LevelFilter::from_u8(MAX_LEVEL.load(Ordering::Relaxed));
        metadata.level() <= max
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let level = record.level();
        let target = record.target();
        let msg = format!("{}", record.args());

        // Write to Android logcat
        let prio = match level {
            log::Level::Error => ANDROID_LOG_ERROR,
            log::Level::Warn => ANDROID_LOG_WARN,
            log::Level::Info => ANDROID_LOG_INFO,
            log::Level::Debug => ANDROID_LOG_DEBUG,
            log::Level::Trace => ANDROID_LOG_VERBOSE,
        };

        let tag = CString::new("EasyTier-JNI").unwrap();
        let text = match CString::new(msg.as_str()) {
            Ok(s) => s,
            Err(_) => return, // skip log messages with interior null bytes
        };
        unsafe {
            __android_log_write(prio, tag.as_ptr(), text.as_ptr());
        }

        // Forward to Java callback if set
        let callback_guard = LOG_CALLBACK.lock().unwrap();
        if let Some(cb) = callback_guard.as_ref() {
            let _ = cb.on_log(level, target, &msg);
        }
    }

    fn flush(&self) {}
}

static LOGGER: CallbackLogger = CallbackLogger;

static LOGGER_INIT: Lazy<()> = Lazy::new(|| {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
});

/// Initialise the logger. Must be called before any EasyTier JNI function.
/// Idempotent — safe to call multiple times.
pub(crate) fn init() {
    Lazy::force(&LOGGER_INIT);
}

/// Set the Java log callback. Pass a null JObject to clear.
pub(crate) fn set_log_callback(env: &mut JNIEnv, callback: JObject) -> Result<(), String> {
    let mut guard = LOG_CALLBACK
        .lock()
        .map_err(|e| format!("Failed to lock log callback: {}", e))?;
    if callback.is_null() {
        *guard = None;
        return Ok(());
    }
    let java_vm = env
        .get_java_vm()
        .map_err(|e| format!("Failed to get JavaVM: {:?}", e))?;
    let global_ref = env
        .new_global_ref(&callback)
        .map_err(|e| format!("Failed to create callback global ref: {:?}", e))?;
    *guard = Some(Arc::new(JniLogCallback {
        java_vm,
        callback: global_ref,
    }));
    Ok(())
}

/// Set the maximum log level.
/// level: "off", "error", "warn", "info", "debug", "trace"
pub(crate) fn set_log_level(level: &str) {
    let lf = match level.to_lowercase().as_str() {
        "off" => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Debug,
    };
    MAX_LEVEL.store(lf as u8, Ordering::Relaxed);
    log::set_max_level(lf);
}
