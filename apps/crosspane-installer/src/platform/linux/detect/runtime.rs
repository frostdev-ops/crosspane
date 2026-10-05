//! Structural dependency candidates, not native load, encoder, keyring, or audio health.
use super::{Fact, LibraryFact, ProbeIssue, RuntimeFacts};
use crate::agent_contract::{KeyStoreProvenance, ObservationSource};
use crate::platform::linux::{
    native_io::{
        ChildEnvironment, Deadline, LinuxNativeIo, MAX_ELF_PREFIX_BYTES, NativeError, SystemBytes,
        SystemRead,
    },
    payload::Architecture,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

/// DT_NEEDED entries in one image (the largest real one on the owner's desktop has 37).
pub const MAX_LIBRARIES: usize = 64;
/// Distinct libraries in the whole transitive graph (the release agent's is ~100 on a real desktop).
pub const MAX_GRAPH_LIBRARIES: usize = 512;
pub const MAX_PROGRAM_HEADERS: usize = 128;
pub const MAX_DYNAMIC_ENTRIES: usize = 4096;
pub const MAX_STRING_BYTES: usize = 1024 * 1024;
/// SONAME prefixes of the FFmpeg libraries the release agent declares in DT_NEEDED.
pub const FFMPEG_FAMILY: [&str; 3] = ["libavcodec.so.", "libavutil.so.", "libswscale.so."];

#[derive(Debug)]
pub struct RuntimeInput<'a> {
    pub architecture: Architecture,
    pub features: &'a [String],
    /// The whole caller-verified staged/installed agent image (at most `MAX_ELF_PREFIX_BYTES`),
    /// not a header prefix. Acquisition belongs to integration, not this parser.
    pub agent_elf: &'a [u8],
    /// Only the caller's identity-matched decoded agent observation, never inferred from a bus name.
    pub keystore: Option<KeyStoreProvenance>,
}

pub trait RuntimeReader {
    fn source(&self) -> ObservationSource;
    fn library(&self, soname: &str, deadline: &Deadline) -> Result<SystemBytes, NativeError>;
    fn secret_service(&self, deadline: &Deadline) -> Result<bool, NativeError>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfDependencies {
    pub needed: Vec<String>,
    pub soname: Option<String>,
}
fn region(bytes: &[u8], offset: u64, length: u64) -> Result<&[u8], ProbeIssue> {
    let end = offset.checked_add(length).ok_or(ProbeIssue::Malformed)?;
    bytes
        .get(
            usize::try_from(offset).map_err(|_| ProbeIssue::Malformed)?
                ..usize::try_from(end).map_err(|_| ProbeIssue::Malformed)?,
        )
        .ok_or(ProbeIssue::Malformed)
}
fn number(bytes: &[u8], offset: usize, size: usize) -> Result<u64, ProbeIssue> {
    let part = region(bytes, offset as u64, size as u64)?;
    Ok(part
        .iter()
        .enumerate()
        .fold(0, |n, (i, b)| n | (u64::from(*b) << (8 * i))))
}
fn bare(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && !name.contains("..")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
}
fn string(table: &[u8], offset: u64) -> Result<String, ProbeIssue> {
    let tail = table
        .get(usize::try_from(offset).map_err(|_| ProbeIssue::Malformed)?..)
        .ok_or(ProbeIssue::Malformed)?;
    let end = tail
        .iter()
        .position(|b| *b == 0)
        .ok_or(ProbeIssue::Malformed)?;
    let name = std::str::from_utf8(&tail[..end]).map_err(|_| ProbeIssue::Malformed)?;
    if !bare(name) {
        return Err(ProbeIssue::Malformed);
    }
    Ok(name.into())
}
fn metadata_mapping(
    loads: &[(u64, u64, u64, u64)],
    address: u64,
    size: u64,
) -> Result<u64, ProbeIssue> {
    let end = address.checked_add(size).ok_or(ProbeIssue::Malformed)?;
    let mut mapped = None;
    for (base, file_size, offset, memory_size) in loads {
        if address < base + memory_size && *base < end {
            if mapped.is_some() {
                return Err(ProbeIssue::Ambiguous);
            }
            // Any partial/zero-fill overlap also prevents one unambiguous file-backed range.
            if address < *base || end > base + file_size {
                return Err(ProbeIssue::Malformed);
            }
            mapped = Some(
                offset
                    .checked_add(address - base)
                    .ok_or(ProbeIssue::Malformed)?,
            );
        }
    }
    mapped.ok_or(ProbeIssue::Malformed)
}
/// ELF64 little-endian x86_64/aarch64 PT_DYNAMIC/DT_NEEDED only. No loader or symbol execution.
/// `bytes` is the whole image: the program headers come from its start, then exactly the
/// PT_DYNAMIC range and the referenced DT_STRTAB range (each size-bounded) wherever they lie.
/// Missing/truncated metadata, nonstandard search paths, and unsupported images never resolve.
pub fn elf_dependencies(
    bytes: &[u8],
    architecture: Architecture,
) -> Result<ElfDependencies, ProbeIssue> {
    if bytes.len() > MAX_ELF_PREFIX_BYTES {
        return Err(ProbeIssue::Oversize);
    }
    if bytes.len() < 64
        || &bytes[..7] != b"\x7fELF\x02\x01\x01"
        || ![0, 3].contains(&bytes[7])
        || bytes[8] != 0
        || ![2, 3].contains(&number(bytes, 16, 2)?)
        || number(bytes, 18, 2)?
            != match architecture {
                Architecture::X86_64 => 62,
                Architecture::Aarch64 => 183,
            }
        || number(bytes, 20, 4)? != 1
        || number(bytes, 52, 2)? != 64
        || number(bytes, 54, 2)? != 56
    {
        return Err(ProbeIssue::Malformed);
    }
    let count = number(bytes, 56, 2)?;
    if count == 0 || count > MAX_PROGRAM_HEADERS as u64 {
        return Err(ProbeIssue::Oversize);
    }
    let headers = region(bytes, number(bytes, 32, 8)?, count * 56)?;
    let mut loads = Vec::new();
    let mut dynamic = None;
    for header in headers.as_chunks::<56>().0 {
        let kind = number(header, 0, 4)?;
        let offset = number(header, 8, 8)?;
        let address = number(header, 16, 8)?;
        let size = number(header, 32, 8)?;
        let memory_size = number(header, 40, 8)?;
        offset.checked_add(size).ok_or(ProbeIssue::Malformed)?;
        address
            .checked_add(memory_size)
            .ok_or(ProbeIssue::Malformed)?;
        if size > memory_size {
            return Err(ProbeIssue::Malformed);
        }
        match kind {
            1 => loads.push((address, size, offset, memory_size)),
            2 => {
                if dynamic.is_some() || size == 0 || size % 16 != 0 {
                    return Err(ProbeIssue::Malformed);
                }
                if size / 16 > MAX_DYNAMIC_ENTRIES as u64 {
                    return Err(ProbeIssue::Oversize);
                }
                dynamic = Some((offset, address, size, region(bytes, offset, size)?));
            }
            _ => {}
        }
    }
    let mut tags = BTreeMap::new();
    let mut needed = Vec::new();
    let mut ended = false;
    let (offset, address, size, dynamic) = dynamic.ok_or(ProbeIssue::Unverified)?;
    if metadata_mapping(&loads, address, size)? != offset {
        return Err(ProbeIssue::Malformed);
    }
    for entry in dynamic.as_chunks::<16>().0 {
        let tag = number(entry, 0, 8)?;
        let value = number(entry, 8, 8)?;
        if tag == 0 {
            ended = true;
            break;
        }
        if tag == 1 {
            if needed.len() == MAX_LIBRARIES {
                return Err(ProbeIssue::Oversize);
            }
            needed.push(value);
        } else if [5, 10, 14].contains(&tag) {
            if tags.insert(tag, value).is_some() {
                return Err(ProbeIssue::Malformed);
            }
        } else if [15, 29, 0x6ffffefb, 0x6ffffefc, 0x7ffffffd, 0x7fffffff].contains(&tag) {
            return Err(ProbeIssue::Unverified); // custom search/audit/filter is outside fixed directories
        }
    }
    if !ended {
        return Err(ProbeIssue::Malformed);
    }
    let address = *tags.get(&5).ok_or(ProbeIssue::Malformed)?;
    let size = *tags.get(&10).ok_or(ProbeIssue::Malformed)?;
    if size == 0 || size > MAX_STRING_BYTES as u64 {
        return Err(ProbeIssue::Oversize);
    }
    let table = region(bytes, metadata_mapping(&loads, address, size)?, size)?;
    let mut unique = BTreeSet::new();
    let needed = needed
        .into_iter()
        .map(|offset| {
            let name = string(table, offset)?;
            if !unique.insert(name.clone()) {
                return Err(ProbeIssue::Malformed);
            }
            Ok(name)
        })
        .collect::<Result<_, _>>()?;
    Ok(ElfDependencies {
        needed,
        soname: tags
            .get(&14)
            .map(|offset| string(table, *offset))
            .transpose()?,
    })
}
fn issue(error: NativeError) -> ProbeIssue {
    match error {
        NativeError::Timeout => ProbeIssue::Timeout,
        NativeError::Cancelled => ProbeIssue::Cancelled,
        NativeError::Oversize => ProbeIssue::Oversize,
        NativeError::Foreign => ProbeIssue::Foreign,
        NativeError::Invalid => ProbeIssue::Malformed,
        _ => ProbeIssue::Unavailable, // unavailable never proves absence
    }
}
struct NativeRuntime {
    io: Arc<LinuxNativeIo>,
    environment: ChildEnvironment,
}
impl RuntimeReader for NativeRuntime {
    fn source(&self) -> ObservationSource {
        self.io.target().source()
    }
    fn library(&self, soname: &str, deadline: &Deadline) -> Result<SystemBytes, NativeError> {
        self.io
            .read_system(SystemRead::Library(soname.into()), deadline)
    }
    fn secret_service(&self, deadline: &Deadline) -> Result<bool, NativeError> {
        let io = self.io.clone();
        let environment = self.environment.clone();
        let task_deadline = deadline.clone();
        bounded_bus(deadline, move || {
            let stream = io.connect_session_bus(&environment, &task_deadline)?;
            let connection = zbus::blocking::connection::Builder::async_io_unix_stream(stream)
                .max_queued(8)
                .method_timeout(Duration::from_millis(500))
                .build()
                .map_err(|_| NativeError::Unavailable)?;
            let result = (|| {
                task_deadline.check()?;
                let reply = connection
                    .call_method(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        Some("org.freedesktop.DBus"),
                        "NameHasOwner",
                        &("org.freedesktop.secrets",),
                    )
                    .map_err(|_| NativeError::Unavailable)?;
                let owned = reply
                    .body()
                    .deserialize::<bool>()
                    .map_err(|_| NativeError::Invalid)?;
                task_deadline.check()?;
                Ok(owned)
            })();
            let _ = connection.close();
            result
        })
    }
}
static BUS_WORKERS: AtomicUsize = AtomicUsize::new(0);
struct BusSlot;
impl Drop for BusSlot {
    fn drop(&mut self) {
        BUS_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
// Auth/Hello can stall before method_timeout applies. Keep admission until the whole task finishes.
fn bounded_bus(
    deadline: &Deadline,
    task: impl FnOnce() -> Result<bool, NativeError> + Send + 'static,
) -> Result<bool, NativeError> {
    deadline.check()?;
    BUS_WORKERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| NativeError::Busy)?;
    let slot = BusSlot;
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("installer-runtime-read".into())
        .spawn(move || {
            let _slot = slot;
            let _ = send.try_send(task());
        })
        .map_err(|_| NativeError::Unavailable)?;
    loop {
        deadline.check()?;
        match receive.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Disconnected) => return Err(NativeError::Unavailable),
            Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
        }
    }
}
pub fn inspect(
    io: Arc<LinuxNativeIo>,
    environment: ChildEnvironment,
    deadline: &Deadline,
    input: RuntimeInput<'_>,
    now_ms: &dyn Fn() -> u64,
) -> RuntimeFacts {
    inspect_with(&NativeRuntime { io, environment }, deadline, input, now_ms)
}
/// Stamp each completed observation with the supplied caller clock before delivery.
/// Supplied manifest/provenance retain caller admission requirements; these stamps do not renew them.
pub fn inspect_with(
    reader: &dyn RuntimeReader,
    deadline: &Deadline,
    input: RuntimeInput<'_>,
    now_ms: &dyn Fn() -> u64,
) -> RuntimeFacts {
    inspect_checked(reader, deadline, input, now_ms, &|| {})
}
fn inspect_checked(
    reader: &dyn RuntimeReader,
    deadline: &Deadline,
    input: RuntimeInput<'_>,
    now_ms: &dyn Fn() -> u64,
    validation_checkpoint: &dyn Fn(),
) -> RuntimeFacts {
    let source = reader.source();
    let fact = |value| Fact {
        value,
        source,
        observed_at_ms: now_ms(),
    };
    let root = elf_dependencies(input.agent_elf, input.architecture);
    let mut pending: VecDeque<_> = root
        .as_ref()
        .map(|elf| elf.needed.clone())
        .unwrap_or_default()
        .into();
    pending.extend(
        [
            "libopus.so.0",
            "libpipewire-0.3.so.0",
            "libxkbcommon.so.0",
            "libwayland-client.so.0",
        ]
        .map(String::from),
    );
    let mut seen = BTreeSet::new();
    let mut libraries = Vec::new();
    let mut graph_issue = root.as_ref().err().copied();
    while let Some(name) = pending.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if seen.len() > MAX_GRAPH_LIBRARIES {
            graph_issue = Some(ProbeIssue::Oversize);
            break;
        }
        let resolution = (|| {
            deadline.check().map_err(issue)?;
            let file = reader.library(&name, deadline).map_err(issue)?;
            deadline.check().map_err(issue)?;
            let elf = elf_dependencies(&file.bytes, input.architecture)?;
            if number(&file.bytes, 16, 2)? != 3 {
                return Err(ProbeIssue::Malformed);
            }
            if elf.soname.as_ref().is_some_and(|soname| soname != &name) {
                return Err(ProbeIssue::Malformed);
            }
            if !["/usr/lib", "/usr/lib64"]
                .iter()
                .any(|root| file.path.parent() == Some(Path::new(root)))
            {
                return Err(ProbeIssue::Foreign);
            }
            validation_checkpoint();
            deadline.check().map_err(issue)?;
            pending.extend(elf.needed);
            Ok(file.path)
        })();
        // An unreadable dependency can conceal further NEEDED entries; absence is then unknown.
        if let Err(error) = &resolution {
            graph_issue.get_or_insert(*error);
        }
        libraries.push(LibraryFact {
            name,
            required: true,
            resolved: Fact {
                value: resolution,
                source,
                observed_at_ms: now_ms(),
            },
        });
    }
    let family = |prefixes: &[&str]| -> Result<bool, ProbeIssue> {
        if let Some(error) = graph_issue {
            return Err(error);
        }
        for prefix in prefixes {
            let matches: Vec<_> = libraries
                .iter()
                .filter(|library| library.name.starts_with(prefix))
                .collect();
            if matches.is_empty() {
                return Ok(false);
            } // complete graph: family not declared, not a failed read
            for library in matches {
                library.resolved.value.as_ref().map_err(|error| *error)?;
            }
        }
        Ok(true)
    };
    let video_feature = if input.features.len() > 32
        || input
            .features
            .iter()
            .any(|feature| feature.len() > 64 || !bare(feature))
    {
        Err(ProbeIssue::Oversize)
    } else {
        Ok(input.features.iter().any(|feature| feature == "video"))
    };
    // Exactly the libav* libraries the release agent links (it doesn't link libavformat).
    let ffmpeg = fact(family(&FFMPEG_FAMILY));
    let software_video = fact(family(&["libavcodec.so.", "libx264.so."]));
    let opus = fact(family(&["libopus.so."]));
    let pipewire_library = fact(family(&["libpipewire-0.3.so."]));
    let xkb = fact(family(&["libxkbcommon.so."]));
    let wayland_library = fact(family(&["libwayland-client.so."]));
    let secret_service = fact(
        deadline
            .check()
            .map_err(issue)
            .and_then(|_| reader.secret_service(deadline).map_err(issue))
            .and_then(|owned| deadline.check().map_err(issue).map(|_| owned)),
    );
    RuntimeFacts {
        libraries,
        video_feature: fact(video_feature),
        ffmpeg,
        opus,
        pipewire_library,
        xkb,
        wayland_library,
        software_video,
        gpu: fact(Err(ProbeIssue::Unverified)),
        libei_required: false,
        pipewire: fact(Err(ProbeIssue::Unverified)),
        session_manager: fact(Err(ProbeIssue::Unverified)),
        secret_service,
        keystore: Fact {
            value: input.keystore.ok_or(ProbeIssue::Unverified),
            source,
            observed_at_ms: now_ms(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::native_io::Cancellation;
    use std::time::Instant;

    fn put(bytes: &mut [u8], offset: usize, size: usize, value: u64) {
        bytes[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
    }
    // Inert ELF metadata only: no host binary, dynamic loading, or executable entry point.
    fn image(names: &[String], soname: Option<&str>, headers: usize, entries: usize) -> Vec<u8> {
        let mut strings = vec![0];
        let mut tags = vec![(5, 0), (10, 0)];
        for name in names {
            tags.push((1, strings.len() as u64));
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        if let Some(name) = soname {
            tags.push((14, strings.len() as u64));
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        tags.push((0, 0));
        tags.resize(entries.max(tags.len()), (0, 0));
        let dynamic = 64 + headers * 56;
        let table = dynamic + tags.len() * 16;
        tags[0].1 = 0x1000 + table as u64;
        tags[1].1 = strings.len() as u64;
        let mut bytes = vec![0; table + strings.len()];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        put(&mut bytes, 16, 2, 3);
        put(&mut bytes, 18, 2, 62);
        put(&mut bytes, 20, 4, 1);
        put(&mut bytes, 32, 8, 64);
        put(&mut bytes, 52, 2, 64);
        put(&mut bytes, 54, 2, 56);
        put(&mut bytes, 56, 2, headers as u64);
        put(&mut bytes, 64, 4, 1);
        put(&mut bytes, 80, 8, 0x1000);
        let length = bytes.len() as u64;
        put(&mut bytes, 96, 8, length);
        put(&mut bytes, 104, 8, length);
        put(&mut bytes, 120, 4, 2);
        put(&mut bytes, 128, 8, dynamic as u64);
        put(&mut bytes, 136, 8, 0x1000 + dynamic as u64);
        put(&mut bytes, 152, 8, (tags.len() * 16) as u64);
        put(&mut bytes, 160, 8, (tags.len() * 16) as u64);
        for (index, (tag, value)) in tags.into_iter().enumerate() {
            put(&mut bytes, dynamic + index * 16, 8, tag);
            put(&mut bytes, dynamic + index * 16 + 8, 8, value);
        }
        bytes[table..].copy_from_slice(&strings);
        bytes
    }
    fn parse(bytes: &[u8]) -> Result<ElfDependencies, ProbeIssue> {
        elf_dependencies(bytes, Architecture::X86_64)
    }
    fn basic() -> Vec<u8> {
        image(&["libopus.so.0".into()], Some("libfixture.so.1"), 2, 0)
    }

    fn overlapping_image(dynamic: bool, partial: bool, zero_fill: bool) -> Vec<u8> {
        let mut bytes = image(&["libopus.so.0".into()], Some("libfixture.so.1"), 3, 0);
        assert!(parse(&bytes).is_ok());
        let start = if dynamic {
            232
        } else {
            number(&bytes, 240, 8).unwrap() as usize - 0x1000
        };
        let size = if dynamic {
            number(&bytes, 152, 8).unwrap() as usize
        } else {
            number(&bytes, 256, 8).unwrap() as usize
        };
        let shift = if partial { size / 2 } else { 0 };
        let alternative = bytes[start + shift..start + size].to_vec();
        bytes.resize(800 + alternative.len(), 0);
        bytes[800..].copy_from_slice(&alternative);
        if dynamic && !partial {
            put(&mut bytes, 840, 8, 14); // Another valid NEEDED name in the existing table.
        } else if !dynamic && !partial {
            bytes[801] = b'X'; // Another valid name with the same bounded string-table layout.
        }
        let length = bytes.len() as u64;
        put(&mut bytes, 96, 8, length);
        put(&mut bytes, 104, 8, length);
        put(&mut bytes, 176, 4, 1);
        put(&mut bytes, 184, 8, 800);
        put(&mut bytes, 192, 8, (0x1000 + start + shift) as u64);
        put(
            &mut bytes,
            208,
            8,
            if zero_fill { 0 } else { (size - shift) as u64 },
        );
        put(&mut bytes, 216, 8, (size - shift) as u64);
        bytes
    }

    #[test]
    fn inert_elf_conflicting_dynamic_mappings_and_zero_fill_are_refused() {
        for partial in [false, true] {
            for zero_fill in [false, true] {
                assert!(matches!(
                    parse(&overlapping_image(true, partial, zero_fill)),
                    Err(ProbeIssue::Ambiguous | ProbeIssue::Malformed)
                ));
            }
        }
    }

    #[test]
    fn inert_elf_conflicting_string_mappings_and_zero_fill_are_refused() {
        for partial in [false, true] {
            for zero_fill in [false, true] {
                assert!(matches!(
                    parse(&overlapping_image(false, partial, zero_fill)),
                    Err(ProbeIssue::Ambiguous | ProbeIssue::Malformed)
                ));
            }
        }
    }

    #[test]
    fn expiry_or_cancellation_during_validation_cannot_enqueue_or_publish_success() {
        struct Reader {
            calls: std::cell::Cell<usize>,
            secrets: std::cell::Cell<usize>,
        }
        impl RuntimeReader for Reader {
            fn source(&self) -> ObservationSource {
                ObservationSource::Demo
            }
            fn library(&self, name: &str, _: &Deadline) -> Result<SystemBytes, NativeError> {
                self.calls.set(self.calls.get() + 1);
                let needed = if name == "libwayland-client.so.0" {
                    vec!["liblate.so.1".into()]
                } else {
                    vec![]
                };
                let bytes = image(&needed, Some(name), 2, 0);
                Ok(SystemBytes {
                    path: Path::new("/usr/lib").join(name),
                    file_size: bytes.len() as u64,
                    bytes,
                })
            }
            fn secret_service(&self, _: &Deadline) -> Result<bool, NativeError> {
                self.secrets.set(self.secrets.get() + 1);
                Ok(true)
            }
        }
        for cancel in [false, true] {
            let reader = Reader {
                calls: std::cell::Cell::new(0),
                secrets: std::cell::Cell::new(0),
            };
            let cancellation = Cancellation::default();
            let deadline = Deadline::new(1000, cancellation.clone()).unwrap();
            let checkpoints = std::cell::Cell::new(0);
            let checkpoint = || {
                checkpoints.set(checkpoints.get() + 1);
                if checkpoints.get() == 4 {
                    if cancel {
                        cancellation.cancel();
                    } else {
                        // Deterministically cross this same operation's deadline at validation.
                        while deadline.check().is_ok() {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
            };
            let bytes = image(&[], None, 2, 0);
            let facts = inspect_checked(
                &reader,
                &deadline,
                RuntimeInput {
                    architecture: Architecture::X86_64,
                    features: &[],
                    agent_elf: &bytes,
                    keystore: None,
                },
                &|| 0,
                &checkpoint,
            );
            let error = if cancel {
                ProbeIssue::Cancelled
            } else {
                ProbeIssue::Timeout
            };
            assert_eq!(checkpoints.get(), 4);
            assert_eq!(reader.calls.get(), 4); // The last validated image's child was not enqueued.
            assert_eq!(reader.secrets.get(), 0);
            assert_eq!(facts.libraries.len(), 4);
            assert_eq!(facts.libraries.last().unwrap().resolved.value, Err(error));
            assert_eq!(facts.opus.value, Err(error));
            assert_eq!(facts.wayland_library.value, Err(error));
            assert_eq!(facts.secret_service.value, Err(error));
        }
    }

    #[test]
    fn inert_elf_needed_soname_architectures_and_exact_limits() {
        let bytes = basic();
        assert_eq!(
            parse(&bytes).unwrap(),
            ElfDependencies {
                needed: vec!["libopus.so.0".into()],
                soname: Some("libfixture.so.1".into())
            }
        );
        let mut arm = bytes.clone();
        put(&mut arm, 18, 2, 183);
        assert!(elf_dependencies(&arm, Architecture::Aarch64).is_ok());
        assert_eq!(parse(&arm), Err(ProbeIssue::Malformed));
        let names = (0..MAX_LIBRARIES)
            .map(|i| format!("libfixture{i}.so.1"))
            .collect::<Vec<_>>();
        assert_eq!(
            parse(&image(&names, None, 128, 4096)).unwrap().needed,
            names
        );
        let mut padded = bytes;
        padded.resize(MAX_ELF_PREFIX_BYTES, 0);
        assert!(parse(&padded).is_ok());
        padded.push(0);
        assert_eq!(parse(&padded), Err(ProbeIssue::Oversize));
        for size in [MAX_STRING_BYTES, MAX_STRING_BYTES + 1] {
            let mut padded = basic();
            let table = number(&padded, 184, 8).unwrap() as usize - 0x1000;
            padded.resize(table + size, 0);
            let length = padded.len() as u64;
            put(&mut padded, 96, 8, length);
            put(&mut padded, 104, 8, length);
            put(&mut padded, 200, 8, size as u64);
            if size == MAX_STRING_BYTES {
                assert!(parse(&padded).is_ok());
            } else {
                assert_eq!(parse(&padded), Err(ProbeIssue::Oversize));
            }
        }
        assert_eq!(parse(&image(&[], None, 129, 0)), Err(ProbeIssue::Oversize));
        assert_eq!(parse(&image(&[], None, 2, 4097)), Err(ProbeIssue::Oversize));
        let names = (0..=MAX_LIBRARIES)
            .map(|i| format!("libfixture{i}.so.1"))
            .collect::<Vec<_>>();
        assert_eq!(parse(&image(&names, None, 2, 0)), Err(ProbeIssue::Oversize));
    }

    /// Moves the PT_DYNAMIC and DT_STRTAB bytes of `image` to `at`, as a real linker places them
    /// in the data segment near the end of a large binary, and maps them with a second PT_LOAD.
    fn relocated(names: &[String], at: usize) -> Vec<u8> {
        let mut bytes = image(names, Some("libfixture.so.1"), 3, 0);
        let dynamic = number(&bytes, 128, 8).unwrap() as usize; // PT_DYNAMIC p_offset
        let metadata = bytes[dynamic..].to_vec(); // dynamic entries, then the string table
        let base = 0x4000_0000u64;
        bytes.truncate(dynamic);
        bytes.resize(at, 0);
        bytes.extend_from_slice(&metadata);
        let length = (bytes.len() - at) as u64;
        // PT_LOAD 0 keeps only the headers; header 2 becomes a PT_LOAD of the moved metadata.
        put(&mut bytes, 96, 8, dynamic as u64);
        put(&mut bytes, 104, 8, dynamic as u64);
        put(&mut bytes, 128, 8, at as u64);
        put(&mut bytes, 136, 8, base);
        put(&mut bytes, 176, 4, 1);
        put(&mut bytes, 184, 8, at as u64);
        put(&mut bytes, 192, 8, base);
        put(&mut bytes, 208, 8, length);
        put(&mut bytes, 216, 8, length);
        let table = base + (number(&bytes, at + 8, 8).unwrap() - 0x1000 - dynamic as u64);
        put(&mut bytes, at + 8, 8, table); // DT_STRTAB moves with the metadata
        bytes
    }

    #[test]
    fn dependency_metadata_far_beyond_a_header_prefix_resolves() {
        // The real stripped release agent (34 MB) has PT_DYNAMIC at 0x19242a8 (~25 MiB).
        let names = ["libavcodec.so.63", "libopus.so.0", "libc.so.6"].map(String::from);
        for at in [0x19242a8, MAX_ELF_PREFIX_BYTES - 4096] {
            let bytes = relocated(&names, at);
            assert!(bytes.len() > 4 * 1024 * 1024);
            assert_eq!(
                parse(&bytes),
                Ok(ElfDependencies {
                    needed: names.to_vec(),
                    soname: Some("libfixture.so.1".into())
                }),
                "{at:#x}"
            );
            // Every bound and mapping check still applies far into the image.
            let mut truncated = bytes.clone();
            truncated.truncate(truncated.len() - 1);
            assert!(parse(&truncated).is_err());
            let mut unmapped = bytes.clone();
            put(&mut unmapped, 136, 8, 0x900000); // PT_DYNAMIC outside every PT_LOAD
            assert!(parse(&unmapped).is_err());
            let mut search_path = bytes.clone();
            put(&mut search_path, at + 32, 8, 29); // DT_NEEDED -> DT_RUNPATH
            assert_eq!(parse(&search_path), Err(ProbeIssue::Unverified));
        }
        let mut oversize = relocated(&names, 0x19242a8);
        oversize.resize(MAX_ELF_PREFIX_BYTES + 1, 0);
        assert_eq!(parse(&oversize), Err(ProbeIssue::Oversize));
    }

    #[test]
    fn graph_admits_a_real_desktop_sized_closure_and_still_bounds_it() {
        struct Reader;
        impl RuntimeReader for Reader {
            fn source(&self) -> ObservationSource {
                ObservationSource::Demo
            }
            fn library(&self, name: &str, _: &Deadline) -> Result<SystemBytes, NativeError> {
                // A chain libchainN -> libchainN+1 -> ... ending at MAX_GRAPH_LIBRARIES + 3.
                let next = name
                    .strip_prefix("libchain")
                    .and_then(|n| n.strip_suffix(".so.1"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n < MAX_GRAPH_LIBRARIES + 3)
                    .map(|n| vec![format!("libchain{}.so.1", n + 1)])
                    .unwrap_or_default();
                let bytes = image(&next, Some(name), 2, 0);
                Ok(SystemBytes {
                    path: Path::new("/usr/lib").join(name),
                    file_size: bytes.len() as u64,
                    bytes,
                })
            }
            fn secret_service(&self, _: &Deadline) -> Result<bool, NativeError> {
                Ok(true)
            }
        }
        let run = |chain: usize| {
            let start = MAX_GRAPH_LIBRARIES + 4 - chain;
            let bytes = image(&[format!("libchain{start}.so.1")], None, 2, 0);
            let deadline = Deadline::new(10_000, Cancellation::default()).unwrap();
            inspect_with(
                &Reader,
                &deadline,
                RuntimeInput {
                    architecture: Architecture::X86_64,
                    features: &[],
                    agent_elf: &bytes,
                    keystore: None,
                },
                &|| 0,
            )
        };
        // libchainN for N in start..=MAX+3 is `chain` libraries, then the four fixed ones.
        let facts = run(MAX_GRAPH_LIBRARIES - 4);
        assert_eq!(facts.libraries.len(), MAX_GRAPH_LIBRARIES);
        assert_eq!(facts.opus.value, Ok(true));
        let facts = run(MAX_GRAPH_LIBRARIES - 3);
        assert_eq!(facts.opus.value, Err(ProbeIssue::Oversize));
    }

    #[test]
    fn inert_elf_rejects_truncation_namespace_duplicate_and_unmapped_metadata() {
        let bytes = basic();
        for end in [0, 7, 63, 119, 175, bytes.len() - 1] {
            assert!(parse(&bytes[..end]).is_err(), "truncation at {end}");
        }
        for name in [
            "",
            "../lib.so",
            "sub/lib.so",
            "/lib.so",
            "$ORIGIN",
            "bad\nname",
        ] {
            assert_eq!(
                parse(&image(&[name.into()], None, 2, 0)),
                Err(ProbeIssue::Malformed)
            );
        }
        assert_eq!(
            parse(&image(&["lib.so".into(), "lib.so".into()], None, 2, 0)),
            Err(ProbeIssue::Malformed)
        );
        for (offset, size, value) in [
            (5, 1, 2),          // big endian
            (16, 2, 1),         // relocatable
            (32, 8, u64::MAX),  // overflowing headers
            (136, 8, 0x900000), // PT_DYNAMIC outside PT_LOAD
            (184, 8, u64::MAX), // unmapped DT_STRTAB
            (200, 8, MAX_STRING_BYTES as u64 + 1),
            (232, 8, u64::MAX), // invalid DT_SONAME string index
            (240, 8, 30),       // no DT_NULL terminator
        ] {
            let mut invalid = bytes.clone();
            put(&mut invalid, offset, size, value);
            assert!(parse(&invalid).is_err(), "field at {offset}");
        }
        for tag in [15, 29, 0x6ffffefb, 0x6ffffefc, 0x7ffffffd, 0x7fffffff] {
            let mut invalid = bytes.clone();
            put(&mut invalid, 208, 8, tag);
            assert_eq!(parse(&invalid), Err(ProbeIssue::Unverified));
        }
        let mut duplicate = bytes;
        put(&mut duplicate, 208, 8, 5);
        assert_eq!(parse(&duplicate), Err(ProbeIssue::Malformed));
    }

    #[test]
    fn bounded_bus_retains_noncooperative_slots_and_discards_late_results() {
        let start = Instant::now();
        let mut release = Vec::new();
        for _ in 0..4 {
            let (send, receive) = mpsc::sync_channel(1);
            release.push(send);
            let deadline = Deadline::new(10, Cancellation::default()).unwrap();
            assert_eq!(
                bounded_bus(&deadline, move || {
                    receive.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(false)
                }),
                Err(NativeError::Timeout)
            );
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        let deadline = Deadline::new(100, Cancellation::default()).unwrap();
        assert_eq!(
            bounded_bus(&deadline, || panic!("saturated")),
            Err(NativeError::Busy)
        );
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let deadline = Deadline::new(100, cancelled).unwrap();
        assert_eq!(
            bounded_bus(&deadline, || panic!("cancelled")),
            Err(NativeError::Cancelled)
        );
        for send in release {
            send.send(()).unwrap();
        }
        let cleanup = Instant::now() + Duration::from_secs(1);
        while BUS_WORKERS.load(Ordering::Acquire) != 0 && Instant::now() < cleanup {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(BUS_WORKERS.load(Ordering::Acquire), 0);
        let deadline = Deadline::new(100, Cancellation::default()).unwrap();
        assert_eq!(bounded_bus(&deadline, || Ok(true)), Ok(true));
    }
}
