//! The console module (`modules/mod_console`, linked in): it takes the board's serial port through the host and owns all
//! text. This crate only starts it.

#[cfg(target_os = "espidf")]
extern "C" {
    fn mod_console_start() -> i32;
}

/// After [`crate::init`] (with `modules` and, for uploads, `network`): create the console module. 0 or an `esp_err_t`.
#[cfg(target_os = "espidf")]
pub fn start() -> i32 { unsafe { mod_console_start() } }

#[cfg(not(target_os = "espidf"))]
pub fn start() -> i32 { -1 }
