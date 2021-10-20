//! Implementation of construction and extraction of custom linker section records used to store
//! probe information in an object file.
// Copyright 2021 Oxide Computer Company

use std::{
    collections::BTreeMap,
    ffi::CStr,
    ptr::{null, null_mut},
};

#[cfg(feature = "des")]
use std::{fs, path::Path};

use byteorder::{NativeEndian, ReadBytesExt};
use dof::{Probe, Provider, Section};
use libc::{c_void, Dl_info};

#[cfg(feature = "des")]
use goblin::Object;

pub(crate) const PROBE_REC_VERSION: u8 = 1;

/// Extract probe records from the given file, if possible.
///
/// An `Err` is returned if the file not an ELF file, or if parsing the records fails in some way.
/// `None` is returned if the file is valid, but contains no records.
#[cfg(feature = "des")]
pub fn extract_probe_records<P: AsRef<Path>>(file: P) -> Result<Option<Section>, crate::Error> {
    let data = fs::read(file)?;
    match Object::parse(&data).map_err(|_| crate::Error::InvalidFile)? {
        Object::Elf(object) => {
            // Try to find our special `set_dtrace_probes` section from the section headers. These may not
            // exist, e.g., if the file has been stripped. In that case, we look for the special __start
            // and __stop symbols themselves.
            if let Some(section) = object
                .section_headers
                .iter()
                .filter_map(|header| {
                    if let Some(result) = object.shdr_strtab.get(header.sh_name) {
                        match result {
                            Err(_) => Some(Err(crate::Error::InvalidFile)),
                            Ok(name) => {
                                if name == "set_dtrace_probes" {
                                    Some(Ok(header))
                                } else {
                                    None
                                }
                            }
                        }
                    } else {
                        None
                    }
                })
                .next()
            {
                let section = section?;
                let start = section.sh_offset as usize;
                let end = start + (section.sh_size as usize);
                process_section(&data[start..end])
            } else {
                // Failed to look up the section directly, iterate over the symbols.
                let mut bounds = object.syms.iter().filter_map(|symbol| {
                    if let Some(result) = object.strtab.get(symbol.st_name) {
                        match result {
                            Err(_) => Some(Err(crate::Error::InvalidFile)),
                            Ok(name) => {
                                if name == "__start_set_dtrace_probes"
                                    || name == "__stop_set_dtrace_probes"
                                {
                                    Some(Ok(symbol))
                                } else {
                                    None
                                }
                            }
                        }
                    } else {
                        None
                    }
                });
                if let (Some(Ok(start)), Some(Ok(stop))) = (bounds.next(), bounds.next()) {
                    let (start, stop) = (start.st_value as usize, stop.st_value as usize);
                    process_section(&data[start..stop])
                } else {
                    Ok(None)
                }
            }
        }
        Object::Mach(goblin::mach::Mach::Binary(object)) => {
            // Try to find our special `__dtrace_probes` section from the section headers.
            for section in object.segments.sections().flatten() {
                if let Ok((section, data)) = section {
                    if section.sectname.starts_with(b"__dtrace_probes") {
                        return process_section(&data);
                    }
                }
            }

            // Failed to look up the section directly, iterate over the symbols
            if let Some(syms) = object.symbols {
                let mut bounds = syms.iter().filter_map(|symbol| {
                    if let Ok((name, nlist)) = symbol {
                        if name.contains("__dtrace_probes") {
                            Some(nlist.n_value as usize)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });
                if let (Some(start), Some(stop)) = (bounds.next(), bounds.next()) {
                    process_section(&data[start..stop])
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
        _ => Err(crate::Error::InvalidFile),
    }
}

// Extract records for all defined probes from our custom linker sections.
pub(crate) fn process_section(mut data: &[u8]) -> Result<Option<Section>, crate::Error> {
    let mut providers = BTreeMap::new();

    while !data.is_empty() {
        assert!(
            data.len() >= std::mem::size_of::<u32>(),
            "Not enough bytes for length header"
        );
        // Read the length without consuming it
        let mut len_bytes = data;
        let len = len_bytes.read_u32::<NativeEndian>()? as usize;
        let (rec, rest) = data.split_at(len);
        process_rec(&mut providers, &rec)?;
        data = rest;
    }

    Ok(Some(Section {
        providers,
        ..Default::default()
    }))
}

// Convert an address in an object file into a function and file name, if possible.
pub(crate) fn addr_to_info(addr: u64) -> (Option<String>, Option<String>) {
    unsafe {
        let mut info = Dl_info {
            dli_fname: null(),
            dli_fbase: null_mut(),
            dli_sname: null(),
            dli_saddr: null_mut(),
        };
        if libc::dladdr(addr as *const c_void, &mut info as *mut _) == 0 {
            (None, None)
        } else {
            (
                Some(CStr::from_ptr(info.dli_sname).to_string_lossy().to_string()),
                Some(CStr::from_ptr(info.dli_fname).to_string_lossy().to_string()),
            )
        }
    }
}

// Limit a string to the DTrace-imposed maxima. Note that this ensures a null-terminated C string
// result, i.e., the actula string is of length `limit - 1`.
// See dtrace.h
const MAX_PROVIDER_NAME_LEN: usize = 64;
const MAX_PROBE_NAME_LEN: usize = 64;
const MAX_FUNC_NAME_LEN: usize = 128;
const MAX_ARG_TYPE_LEN: usize = 128;
fn limit_string_length<S: AsRef<str>>(s: S, limit: usize) -> String {
    let s = s.as_ref();
    let limit = s.len().min(limit - 1);
    s[..limit].to_string()
}

// Process a single record from the custom linker section.
fn process_rec(providers: &mut BTreeMap<String, Provider>, rec: &[u8]) -> Result<(), crate::Error> {
    // Skip over the length which was already read.
    let mut data = &rec[4..];

    let version = data.read_u8()?;

    // If this record comes from a future version of the data format, we skip it
    // and hope that the author of main will *also* include a call to a more
    // recent version. Note that future versions should handle previous formats.
    if version > PROBE_REC_VERSION {
        return Ok(());
    }

    let n_args = data.read_u8()? as usize;
    let flags = data.read_u16::<NativeEndian>()?;
    let address = data.read_u64::<NativeEndian>()?;
    let provname = data.read_cstr();
    let probename = data.read_cstr();
    let args = {
        let mut args = Vec::with_capacity(n_args);
        for _ in 0..n_args {
            args.push(limit_string_length(data.read_cstr(), MAX_ARG_TYPE_LEN));
        }
        args
    };

    let funcname = match addr_to_info(address).0 {
        Some(s) => limit_string_length(s, MAX_FUNC_NAME_LEN),
        None => format!("?{:#x}", address),
    };

    let provname = limit_string_length(provname, MAX_PROVIDER_NAME_LEN);
    let provider = providers.entry(provname.clone()).or_insert(Provider {
        name: provname,
        probes: BTreeMap::new(),
    });

    let probename = limit_string_length(probename, MAX_PROBE_NAME_LEN);
    let probe = provider.probes.entry(probename.clone()).or_insert(Probe {
        name: probename,
        function: funcname,
        address: address,
        offsets: vec![],
        enabled_offsets: vec![],
        arguments: vec![],
    });
    probe.arguments = args;

    // We expect to get records in address order for a given probe; our offsets
    // would be negative otherwise.
    assert!(address >= probe.address);

    if flags == 0 {
        probe.offsets.push((address - probe.address) as u32);
    } else {
        probe.enabled_offsets.push((address - probe.address) as u32);
    }
    Ok(())
}

trait ReadCstrExt<'a> {
    fn read_cstr(&mut self) -> &'a str;
}

impl<'a> ReadCstrExt<'a> for &'a [u8] {
    fn read_cstr(&mut self) -> &'a str {
        let index = self
            .iter()
            .position(|ch| *ch == 0)
            .expect("ran out of bytes before we found a zero");

        let ret = std::str::from_utf8(&self[..index]).unwrap();
        *self = &self[index + 1..];
        ret
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use byteorder::{NativeEndian, WriteBytesExt};

    use super::process_rec;
    use super::process_section;
    use super::PROBE_REC_VERSION;
    use super::{MAX_PROBE_NAME_LEN, MAX_PROVIDER_NAME_LEN};

    #[test]
    fn test_process_rec() {
        let mut rec = Vec::<u8>::new();

        // write a dummy length
        rec.write_u32::<NativeEndian>(0).unwrap();
        rec.write_u8(PROBE_REC_VERSION).unwrap();
        rec.write_u8(0).unwrap();
        rec.write_u16::<NativeEndian>(0).unwrap();
        rec.write_u64::<NativeEndian>(0x1234).unwrap();
        rec.write_cstr("provider");
        rec.write_cstr("probe");
        // fix the length field
        let len = rec.len();
        (&mut rec[0..])
            .write_u32::<NativeEndian>(len as u32)
            .unwrap();

        let mut providers = BTreeMap::new();
        process_rec(&mut providers, rec.as_slice()).unwrap();

        let probe = providers
            .get("provider")
            .unwrap()
            .probes
            .get("probe")
            .unwrap();

        assert_eq!(probe.name, "probe");
        assert_eq!(probe.address, 0x1234);
    }

    #[test]
    fn test_process_rec_long_names() {
        let mut rec = Vec::<u8>::new();

        // write a dummy length
        let long_name: String = std::iter::repeat("p").take(130).collect();
        rec.write_u32::<NativeEndian>(0).unwrap();
        rec.write_u8(PROBE_REC_VERSION).unwrap();
        rec.write_u8(0).unwrap();
        rec.write_u16::<NativeEndian>(0).unwrap();
        rec.write_u64::<NativeEndian>(0x1234).unwrap();
        rec.write_cstr(&long_name);
        rec.write_cstr(&long_name);
        // fix the length field
        let len = rec.len();
        (&mut rec[0..])
            .write_u32::<NativeEndian>(len as u32)
            .unwrap();

        let mut providers = BTreeMap::new();
        process_rec(&mut providers, rec.as_slice()).unwrap();

        let expected_provider_name = &long_name[..MAX_PROVIDER_NAME_LEN - 1];
        let expected_probe_name = &long_name[..MAX_PROBE_NAME_LEN - 1];

        assert!(providers.get(&long_name).is_none());
        let probe = providers
            .get(expected_provider_name)
            .unwrap()
            .probes
            .get(expected_probe_name)
            .unwrap();

        assert_eq!(probe.name, expected_probe_name);
        assert_eq!(probe.address, 0x1234);
    }

    #[test]
    fn test_process_section() {
        let mut data = Vec::<u8>::new();

        // write a dummy length for the first record
        data.write_u32::<NativeEndian>(0).unwrap();
        data.write_u8(PROBE_REC_VERSION).unwrap();
        data.write_u8(0).unwrap();
        data.write_u16::<NativeEndian>(0).unwrap();
        data.write_u64::<NativeEndian>(0x1234).unwrap();
        data.write_cstr("provider");
        data.write_cstr("probe");
        let len = data.len();
        (&mut data[0..])
            .write_u32::<NativeEndian>(len as u32)
            .unwrap();

        data.write_u32::<NativeEndian>(0).unwrap();
        data.write_u8(PROBE_REC_VERSION).unwrap();
        data.write_u8(0).unwrap();
        data.write_u16::<NativeEndian>(0).unwrap();
        data.write_u64::<NativeEndian>(0x12ab).unwrap();
        data.write_cstr("provider");
        data.write_cstr("probe");
        let len2 = data.len() - len;
        (&mut data[len..])
            .write_u32::<NativeEndian>(len2 as u32)
            .unwrap();

        let section = process_section(data.as_slice()).unwrap().unwrap();

        let probe = section
            .providers
            .get("provider")
            .unwrap()
            .probes
            .get("probe")
            .unwrap();

        assert_eq!(probe.name, "probe");
        assert_eq!(probe.address, 0x1234);
        assert_eq!(probe.offsets, vec![0, 0x12ab - 0x1234]);
    }

    trait WriteCstrExt {
        fn write_cstr(&mut self, s: &str);
    }

    impl WriteCstrExt for Vec<u8> {
        fn write_cstr(&mut self, s: &str) {
            self.extend_from_slice(s.as_bytes());
            self.push(0);
        }
    }
}
