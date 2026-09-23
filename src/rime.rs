use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr::NonNull;

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
    fn rime_fep_create_session(rime: *mut c_void) -> usize;
    fn rime_fep_destroy_session(rime: *mut c_void, session: usize);
    fn rime_fep_process(
        rime: *mut c_void,
        session: usize,
        keycode: c_int,
        modifiers: c_int,
    ) -> c_int;
    fn rime_fep_take_commit(
        rime: *mut c_void,
        session: usize,
        buffer: *mut c_char,
        size: usize,
    ) -> c_int;
    fn rime_fep_status(
        rime: *mut c_void,
        session: usize,
        buffer: *mut c_char,
        size: usize,
    ) -> c_int;
    fn rime_fep_clear(rime: *mut c_void, session: usize);
}

#[derive(Clone, Copy)]
pub struct SessionId(usize);

pub struct RimeEngine {
    handle: NonNull<c_void>,
}

impl RimeEngine {
    pub fn new(
        config_dir: Option<&Path>,
        expected_fingerprint: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let data = rime_data::prepare(config_dir, &version()?)?;
        if data.fingerprint != expected_fingerprint {
            return Err(format!(
                "configuration fingerprint changed while starting server: expected {expected_fingerprint}, got {}",
                data.fingerprint
            )
            .into());
        }

        let shared_data_dir = path_to_cstring(&data.shared_data_dir)?;
        let user_data_dir = path_to_cstring(&data.user_data_dir)?;
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
        if let Err(error) = data.finish() {
            // SAFETY: initialization succeeded, but publishing its cache marker failed.
            unsafe { rime_fep_destroy(handle.as_ptr()) };
            return Err(error);
        }

        Ok(Self { handle })
    }

    pub fn create_session(&mut self) -> Option<SessionId> {
        // SAFETY: the pointer is owned by self and used only on this thread.
        let session = unsafe { rime_fep_create_session(self.handle.as_ptr()) };
        (session != 0).then_some(SessionId(session))
    }

    pub fn destroy_session(&mut self, session: SessionId) {
        // SAFETY: the session belongs to this engine and is destroyed once.
        unsafe { rime_fep_destroy_session(self.handle.as_ptr(), session.0) };
    }

    pub fn process(&mut self, session: SessionId, keycode: i32, modifiers: i32) -> bool {
        // SAFETY: the engine and session are live and used only on this thread.
        unsafe { rime_fep_process(self.handle.as_ptr(), session.0, keycode, modifiers) != 0 }
    }

    pub fn take_commit(&mut self, session: SessionId) -> Vec<u8> {
        let mut buffer = [0 as c_char; BUFFER_SIZE];
        // SAFETY: buffer is writable for BUFFER_SIZE bytes and the session is live.
        let available = unsafe {
            rime_fep_take_commit(
                self.handle.as_ptr(),
                session.0,
                buffer.as_mut_ptr(),
                buffer.len(),
            ) != 0
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

    pub fn status(&mut self, session: SessionId) -> String {
        let mut buffer = [0 as c_char; BUFFER_SIZE];
        // SAFETY: buffer is writable for BUFFER_SIZE bytes and the session is live.
        let available = unsafe {
            rime_fep_status(
                self.handle.as_ptr(),
                session.0,
                buffer.as_mut_ptr(),
                buffer.len(),
            ) != 0
        };
        if !available {
            return "[中]".to_owned();
        }

        // SAFETY: the C shim always writes a terminating NUL.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn clear(&mut self, session: SessionId) {
        // SAFETY: the engine and session are live and used only on this thread.
        unsafe { rime_fep_clear(self.handle.as_ptr(), session.0) };
    }
}

impl Drop for RimeEngine {
    fn drop(&mut self) {
        // SAFETY: the pointer was returned by rime_fep_create and is destroyed once.
        unsafe { rime_fep_destroy(self.handle.as_ptr()) };
    }
}

pub fn version() -> Result<Vec<u8>, Box<dyn Error>> {
    // SAFETY: librime returns a process-lifetime version string.
    let version = unsafe { rime_fep_version() };
    let version = NonNull::new(version.cast_mut()).ok_or("librime has no version")?;
    // SAFETY: the checked pointer refers to a NUL-terminated librime string.
    Ok(unsafe { CStr::from_ptr(version.as_ptr()) }
        .to_bytes()
        .to_vec())
}

fn path_to_cstring(path: &Path) -> Result<CString, std::ffi::NulError> {
    CString::new(path.as_os_str().as_encoded_bytes())
}
