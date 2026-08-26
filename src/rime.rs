use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr::NonNull;

use tempfile::TempDir;

use crate::rime_data;

const BUFFER_SIZE: usize = 8192;

unsafe extern "C" {
    fn rime_fep_create(
        shared_data_dir: *const c_char,
        user_data_dir: *const c_char,
        prebuilt_data_dir: *const c_char,
        deploy: c_int,
        load_plugins: c_int,
    ) -> *mut c_void;
    fn rime_fep_version() -> *const c_char;
    fn rime_fep_destroy(rime: *mut c_void);
    fn rime_fep_process(rime: *mut c_void, keycode: c_int, modifiers: c_int) -> c_int;
    fn rime_fep_take_commit(rime: *mut c_void, buffer: *mut c_char, size: usize) -> c_int;
    fn rime_fep_status(rime: *mut c_void, buffer: *mut c_char, size: usize) -> c_int;
    fn rime_fep_clear(rime: *mut c_void);
}

pub struct Rime {
    handle: NonNull<c_void>,
    _user_data_dir: TempDir,
}

impl Rime {
    pub fn new(config_dir: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: librime returns a process-lifetime version string.
        let version = unsafe { rime_fep_version() };
        let version = NonNull::new(version.cast_mut()).ok_or("librime has no version")?;
        // SAFETY: the checked pointer refers to a NUL-terminated librime string.
        let version = unsafe { CStr::from_ptr(version.as_ptr()) };
        let data = rime_data::prepare(config_dir, version.to_bytes())?;

        let shared_data_dir = path_to_cstring(&data.shared_data_dir)?;
        let user_data_dir = path_to_cstring(data.user_data_dir.path())?;
        let prebuilt_data_dir = path_to_cstring(&data.prebuilt_data_dir)?;
        // SAFETY: all strings are valid for the call; the C shim copies them before returning.
        let handle = unsafe {
            rime_fep_create(
                shared_data_dir.as_ptr(),
                user_data_dir.as_ptr(),
                prebuilt_data_dir.as_ptr(),
                c_int::from(data.deploy),
                c_int::from(config_dir.is_some()),
            )
        };
        let handle = NonNull::new(handle).ok_or("librime initialization failed")?;
        let user_data_dir = match data.finish() {
            Ok(user_data_dir) => user_data_dir,
            Err(error) => {
                // SAFETY: initialization succeeded, but publishing its cache marker failed.
                unsafe { rime_fep_destroy(handle.as_ptr()) };
                return Err(error);
            }
        };

        Ok(Self {
            handle,
            _user_data_dir: user_data_dir,
        })
    }

    pub fn process(&mut self, keycode: i32, modifiers: i32) -> bool {
        // SAFETY: the pointer is owned by self and used only on this thread.
        unsafe { rime_fep_process(self.handle.as_ptr(), keycode, modifiers) != 0 }
    }

    pub fn take_commit(&mut self) -> Vec<u8> {
        let mut buffer = [0 as c_char; BUFFER_SIZE];
        // SAFETY: buffer is writable for BUFFER_SIZE bytes and self owns the session.
        let available = unsafe {
            rime_fep_take_commit(self.handle.as_ptr(), buffer.as_mut_ptr(), buffer.len()) != 0
        };

        if available {
            // SAFETY: the C shim always writes a terminating NUL.
            unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_bytes()
                .to_vec()
        } else {
            Vec::new()
        }
    }

    pub fn status(&self) -> String {
        let mut buffer = [0 as c_char; BUFFER_SIZE];
        // SAFETY: buffer is writable for BUFFER_SIZE bytes and self owns the session.
        let available = unsafe {
            rime_fep_status(self.handle.as_ptr(), buffer.as_mut_ptr(), buffer.len()) != 0
        };
        if !available {
            return "[中]".to_owned();
        }

        // SAFETY: the C shim always writes a terminating NUL.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn clear(&mut self) {
        // SAFETY: self owns a live session.
        unsafe { rime_fep_clear(self.handle.as_ptr()) };
    }
}

impl Drop for Rime {
    fn drop(&mut self) {
        // SAFETY: the pointer was returned by rime_fep_create and is destroyed once.
        unsafe { rime_fep_destroy(self.handle.as_ptr()) };
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, std::ffi::NulError> {
    CString::new(path.as_os_str().as_encoded_bytes())
}
