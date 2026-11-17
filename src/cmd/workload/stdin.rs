// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Raw Windows console text without the standard reader's Ctrl-Z EOF policy.

#[derive(Default)]
struct Utf16 {
    high: Option<u16>,
}

impl Utf16 {
    fn decode(&mut self, units: &[u16]) -> Vec<u8> {
        let mut units: Vec<_> = self
            .high
            .take()
            .into_iter()
            .chain(units.iter().copied())
            .collect();
        if units
            .last()
            .is_some_and(|unit| (0xD800..0xDC00).contains(unit))
        {
            self.high = units.pop();
        }
        String::from_utf16_lossy(&units).into_bytes()
    }
}

#[cfg(windows)]
pub(super) struct Input {
    stdin: std::io::StdinLock<'static>,
    raw: bool,
    decoder: Utf16,
    ready: std::io::Cursor<Vec<u8>>,
}

#[cfg(windows)]
impl Input {
    pub(super) fn new(stdin: std::io::StdinLock<'static>, raw: bool) -> Self {
        Self {
            stdin,
            raw,
            decoder: Utf16::default(),
            ready: std::io::Cursor::new(Vec::new()),
        }
    }
}

#[cfg(windows)]
impl std::io::Read for Input {
    #[allow(
        unsafe_code,
        reason = "Private ReadConsoleW boundary preserves raw console text"
    )]
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, GetLastError, SetLastError};
        if !self.raw {
            return self.stdin.read(bytes);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.ready.read(bytes)?;
            if count != 0 {
                return Ok(count);
            }
            let mut units = [0_u16; 2048];
            let mut count = 0;
            // SAFETY: the locked stdin owns the borrowed handle's lifetime;
            // both output buffers remain valid for this synchronous call.
            // Null input control avoids Rust's Ctrl-Z wakeup/stripping policy.
            let (success, error) = unsafe {
                SetLastError(0);
                let success = windows_sys::Win32::System::Console::ReadConsoleW(
                    self.stdin.as_raw_handle(),
                    units.as_mut_ptr().cast(),
                    units.len() as u32,
                    &raw mut count,
                    std::ptr::null(),
                );
                (success, GetLastError())
            };
            if success == 0 {
                return Err(std::io::Error::from_raw_os_error(error.cast_signed()));
            }
            if count == 0 {
                // Match Rust's reader for an interrupted console read. Other
                // zero-length reads keep their usual EOF meaning.
                if error == ERROR_OPERATION_ABORTED {
                    continue;
                }
                return Ok(0);
            }
            self.ready = std::io::Cursor::new(self.decoder.decode(&units[..count as usize]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Utf16;

    #[test]
    fn control_bytes_and_unicode_survive_every_utf16_boundary() {
        let text = "\0\u{1a}é🦔\r\n";
        let units: Vec<_> = text.encode_utf16().collect();
        for split in 0..=units.len() {
            let mut decoder = Utf16::default();
            let mut bytes = decoder.decode(&units[..split]);
            bytes.extend(decoder.decode(&units[split..]));
            assert_eq!(bytes, text.as_bytes(), "split at {split}");
        }
    }

    #[test]
    fn an_unpaired_surrogate_does_not_discard_the_following_character() {
        let mut decoder = Utf16::default();
        assert!(decoder.decode(&[0xD800]).is_empty());
        assert_eq!(decoder.decode(&[b'a'.into(), 0xDC00]), "�a�".as_bytes());
    }
}
