use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{
    BOOL, CloseHandle, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS,
    ERROR_JOURNAL_NOT_ACTIVE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_RECORD_V2,
};
use windows::Win32::UI::Shell::IsUserAnAdmin;
use windows::core::{HRESULT, PCWSTR};

use crate::index::{FileEntry, JournalBatch, JournalChange, MftSnapshot};

pub fn normalize_drive(raw: &str) -> Result<String> {
    let drive = raw.trim().trim_end_matches('\\').trim_end_matches(':');
    if drive.len() != 1 || !drive.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(anyhow!("unidad invalida: {raw}. Usa por ejemplo C o C:"));
    }
    Ok(format!("{}:", drive.to_ascii_uppercase()))
}

pub fn is_running_as_admin() -> bool {
    unsafe { IsUserAnAdmin() == BOOL(1) }
}

pub fn enumerate_mft(drive: &str) -> Result<MftSnapshot> {
    let mut entries = HashMap::new();
    let volume = open_volume(drive)?;
    let _guard = HandleGuard(volume);
    let journal_data = query_journal_data(volume)?;

    let mut enum_data = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: i64::MAX,
    };
    let mut buffer = vec![0u8; 1024 * 1024];

    loop {
        let mut bytes_returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                volume,
                FSCTL_ENUM_USN_DATA,
                Some((&mut enum_data as *mut MFT_ENUM_DATA_V0).cast::<c_void>()),
                size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr().cast::<c_void>()),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if let Err(err) = ok {
            if err.code() == HRESULT::from_win32(ERROR_HANDLE_EOF.0) {
                break;
            }
            return Err(anyhow!("FSCTL_ENUM_USN_DATA fallo: {err}"));
        }

        if bytes_returned <= 8 {
            break;
        }

        enum_data.StartFileReferenceNumber =
            u64::from_ne_bytes(buffer[0..8].try_into().expect("buffer header invalido"));

        let mut offset = 8usize;
        while offset + size_of::<USN_RECORD_V2>() <= bytes_returned as usize {
            let record = unsafe { &*(buffer.as_ptr().add(offset).cast::<USN_RECORD_V2>()) };
            if record.RecordLength == 0 {
                break;
            }

            let name = decode_usn_name(buffer.as_ptr(), offset, record)?;
            let frn = record.FileReferenceNumber;
            let parent_frn = record.ParentFileReferenceNumber;
            entries.insert(
                frn,
                FileEntry {
                    frn,
                    parent_frn,
                    name,
                    attributes: record.FileAttributes,
                },
            );
            offset += record.RecordLength as usize;
        }
    }

    Ok(MftSnapshot {
        entries,
        next_usn: journal_data.NextUsn,
        journal_id: journal_data.UsnJournalID,
    })
}

pub fn collect_changes(drive: &str, journal_id: u64, start_usn: i64) -> Result<JournalBatch> {
    let volume = open_volume(drive)?;
    let _guard = HandleGuard(volume);
    let current = query_journal_data(volume)?;

    if current.UsnJournalID != journal_id {
        return Err(anyhow!(
            "el journal cambio (id previo {journal_id}, actual {})",
            current.UsnJournalID
        ));
    }

    let mut read_data = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: start_usn,
        ReasonMask: 0xFFFF_FFFF,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: journal_id,
    };
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut bytes_returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            volume,
            FSCTL_READ_USN_JOURNAL,
            Some((&mut read_data as *mut READ_USN_JOURNAL_DATA_V0).cast::<c_void>()),
            size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            buffer.len() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };
    if let Err(err) = ok {
        let code = err.code();
        if code == HRESULT::from_win32(ERROR_JOURNAL_NOT_ACTIVE.0)
            || code == HRESULT::from_win32(ERROR_JOURNAL_DELETE_IN_PROGRESS.0)
        {
            return Err(anyhow!("USN Journal no activo en el volumen: {err}"));
        }
        return Err(anyhow!("FSCTL_READ_USN_JOURNAL fallo: {err}"));
    }

    let mut events = Vec::new();
    let mut next_usn = start_usn;

    if bytes_returned >= 8 {
        next_usn = i64::from_ne_bytes(buffer[0..8].try_into().expect("header USN invalido"));
        let mut offset = 8usize;
        while offset + size_of::<USN_RECORD_V2>() <= bytes_returned as usize {
            let record = unsafe { &*(buffer.as_ptr().add(offset).cast::<USN_RECORD_V2>()) };
            if record.RecordLength == 0 {
                break;
            }
            let reason = record.Reason;
            if (reason & USN_REASON_FILE_DELETE) != 0 {
                events.push(JournalChange::Delete(record.FileReferenceNumber));
            } else if (reason & USN_REASON_FILE_CREATE) != 0
                || (reason & USN_REASON_RENAME_NEW_NAME) != 0
            {
                let name = decode_usn_name(buffer.as_ptr(), offset, record)?;
                events.push(JournalChange::Upsert(FileEntry {
                    frn: record.FileReferenceNumber,
                    parent_frn: record.ParentFileReferenceNumber,
                    name,
                    attributes: record.FileAttributes,
                }));
            }
            offset += record.RecordLength as usize;
        }
    }

    Ok(JournalBatch {
        events,
        next_usn,
        journal_id: current.UsnJournalID,
    })
}

fn query_journal_data(volume: HANDLE) -> Result<USN_JOURNAL_DATA_V0> {
    let mut journal = USN_JOURNAL_DATA_V0::default();
    let mut bytes_returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            volume,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some((&mut journal as *mut USN_JOURNAL_DATA_V0).cast::<c_void>()),
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };
    if let Err(err) = ok {
        return Err(anyhow!("FSCTL_QUERY_USN_JOURNAL fallo: {err}"));
    }
    Ok(journal)
}

fn decode_usn_name(base_ptr: *const u8, offset: usize, record: &USN_RECORD_V2) -> Result<String> {
    let name_len = (record.FileNameLength / 2) as usize;
    let name_offset = offset + record.FileNameOffset as usize;
    let name_ptr = unsafe { base_ptr.add(name_offset).cast::<u16>() };
    let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
    String::from_utf16(name_slice).context("nombre UTF-16 invalido en USN_RECORD")
}

fn open_volume(drive: &str) -> Result<HANDLE> {
    let volume_path = format!("\\\\.\\{drive}");
    let path = to_utf16_null(&volume_path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .with_context(|| format!("no se pudo abrir volumen {volume_path}"))?;

    if handle == INVALID_HANDLE_VALUE {
        return Err(anyhow!("handle invalido al abrir {volume_path}"));
    }
    Ok(handle)
}

fn to_utf16_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
