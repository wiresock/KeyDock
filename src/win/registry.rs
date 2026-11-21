//! Small registry helpers.

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegGetValueW, RegLoadMUIStringW, RegOpenKeyExW, HKEY, KEY_READ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// An open registry key, closed on drop.
pub struct Key(HKEY);

impl Key {
    pub fn open(root: HKEY, path: &str) -> Option<Key> {
        let p = wide(path);
        let mut h = HKEY::default();
        // SAFETY: valid NUL-terminated path and out pointer.
        let rc = unsafe { RegOpenKeyExW(root, PCWSTR(p.as_ptr()), Some(0), KEY_READ, &mut h) };
        (rc == ERROR_SUCCESS).then_some(Key(h))
    }

    /// Subkey names; `Err` if enumeration stops for any reason other than the end.
    pub fn subkeys_checked(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            // SAFETY: buffer and length describe the same storage.
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    i,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("RegEnumKeyExW failed (error {})", rc.0));
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        Ok(out)
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            // SAFETY: buffer and length describe the same storage.
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    i,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if rc != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        out
    }

    pub fn value_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            // SAFETY: buffer and length describe the same storage; data not requested.
            let rc = unsafe {
                RegEnumValueW(
                    self.0,
                    i,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if rc != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        out
    }

    /// All values with their type and raw data. `Err` if enumeration fails
    /// for any reason other than reaching the end.
    pub fn values(&self) -> Result<Vec<(String, u32, Vec<u8>)>, String> {
        let mut out = Vec::new();
        let mut name = vec![0u16; 1024];
        let mut data = vec![0u8; 4096];
        let mut i = 0u32;
        loop {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut ty = 0u32;
            // SAFETY: buffers and lengths describe the same storage.
            let rc = unsafe {
                RegEnumValueW(
                    self.0,
                    i,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut ty),
                    Some(data.as_mut_ptr()),
                    Some(&mut data_len),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                return Ok(out);
            }
            if rc == ERROR_MORE_DATA {
                if data.len() >= 1 << 20 || name.len() >= 32_768 {
                    return Err("registry value too large".into());
                }
                data.resize(data.len() * 4, 0);
                name.resize((name.len() * 4).min(32_768), 0);
                continue;
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("RegEnumValueW failed (error {})", rc.0));
            }
            out.push((
                String::from_utf16_lossy(&name[..name_len as usize]),
                ty,
                data[..data_len as usize].to_vec(),
            ));
            i += 1;
        }
    }

    /// Reads a REG_DWORD value.
    pub fn dword(&self, value: &str) -> Option<u32> {
        let v = wide(value);
        let mut data = 0u32;
        let mut size = 4u32;
        // SAFETY: a 4-byte buffer; size in bytes.
        let rc = unsafe {
            RegGetValueW(
                self.0,
                PCWSTR::null(),
                PCWSTR(v.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut data as *mut u32).cast()),
                Some(&mut size),
            )
        };
        (rc == ERROR_SUCCESS).then_some(data)
    }

    pub fn string(&self, value: &str) -> Option<String> {
        let v = wide(value);
        let mut buf = vec![0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: buffer size in bytes.
        let rc = unsafe {
            RegGetValueW(
                self.0,
                PCWSTR::null(),
                PCWSTR(v.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        (rc == ERROR_SUCCESS).then(|| from_wide(&buf))
    }

    /// Reads a (possibly indirect, `@dll,-id`) localized string value.
    pub fn mui_string(&self, value: &str) -> Option<String> {
        let v = wide(value);
        let mut buf = vec![0u16; 512];
        // SAFETY: buffer size in bytes.
        let rc = unsafe {
            RegLoadMUIStringW(
                self.0,
                PCWSTR(v.as_ptr()),
                Some(PWSTR(buf.as_mut_ptr())),
                (buf.len() * 2) as u32,
                None,
                0,
                PCWSTR::null(),
            )
        };
        (rc == ERROR_SUCCESS).then(|| from_wide(&buf)).filter(|s| !s.is_empty())
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: key opened in `open`.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
