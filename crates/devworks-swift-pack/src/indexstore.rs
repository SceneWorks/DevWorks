//! Minimal reader for the C API shipped with the qualified Apple toolchain.
//! ABI declarations are from swiftlang/llvm-project clang/include/indexstore/indexstore.h.
use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

const INVALID: &str = "swift-indexstore-invalid";
const MAX_OCCURRENCES: usize = 120_000;
type Object = *mut c_void;
type Callback<T> = unsafe extern "C" fn(Object, T) -> bool;
type Closure<T> = unsafe extern "C" fn(Object, Object, Callback<T>) -> bool;
type OpenStore = unsafe extern "C" fn(*const c_char, *mut Object) -> Object;
type OpenReader = unsafe extern "C" fn(Object, *const c_char, *mut Object) -> Object;
type Dispose = unsafe extern "C" fn(Object);
type GetText = unsafe extern "C" fn(Object) -> CText;
type GetObject = unsafe extern "C" fn(Object) -> Object;
type GetBits = unsafe extern "C" fn(Object) -> u64;
type GetKind = unsafe extern "C" fn(Object) -> c_uint;

#[derive(Clone, Copy)]
#[repr(C)]
struct CText {
    data: *const u8,
    length: usize,
}

impl CText {
    fn string(self) -> Result<String, &'static str> {
        if self.length == 0 {
            return Ok(String::new());
        }
        if self.length > 4096 || self.data.is_null() {
            return Err(INVALID);
        }
        // The library owns the string for the lifetime of the immediate reader callback.
        let bytes = unsafe { std::slice::from_raw_parts(self.data, self.length) };
        std::str::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0'))
            .map(str::to_owned)
            .ok_or(INVALID)
    }
}

macro_rules! api {
    ($($field:ident: $kind:ty = $name:literal $(,)?)+) => {
        struct Api {
            _library: libloading::Library,
            $($field: $kind,)+
        }

        impl Api {
            fn open(path: &Path) -> Result<Self, &'static str> {
                // Only the canonical SDK-bound libIndexStore path is passed by the decoder.
                let library = unsafe { libloading::Library::new(path) }.map_err(|_| INVALID)?;
                let api = Self {
                    $($field: unsafe { *library.get::<$kind>($name).map_err(|_| INVALID)? },)+
                    _library: library,
                };
                if unsafe { (api.format)() } != 5 || unsafe { (api.version)() } != 16 {
                    return Err(INVALID);
                }
                Ok(api)
            }
        }
    };
}

api! {
    format: unsafe extern "C" fn() -> c_uint = b"indexstore_format_version",
    version: unsafe extern "C" fn() -> c_uint = b"indexstore_version",
    open_store: OpenStore = b"indexstore_store_create",
    close_store: Dispose = b"indexstore_store_dispose",
    units: unsafe extern "C" fn(Object, c_uint, Object, Callback<CText>) -> bool = b"indexstore_store_units_apply_f",
    open_unit: OpenReader = b"indexstore_unit_reader_create",
    close_unit: Dispose = b"indexstore_unit_reader_dispose",
    system_unit: unsafe extern "C" fn(Object) -> bool = b"indexstore_unit_reader_is_system_unit",
    module: GetText = b"indexstore_unit_reader_get_module_name",
    deps: Closure<Object> = b"indexstore_unit_reader_dependencies_apply_f",
    dep_kind: GetKind = b"indexstore_unit_dependency_get_kind",
    dep_system: unsafe extern "C" fn(Object) -> bool = b"indexstore_unit_dependency_is_system",
    dep_file: GetText = b"indexstore_unit_dependency_get_filepath",
    dep_name: GetText = b"indexstore_unit_dependency_get_name",
    open_record: OpenReader = b"indexstore_record_reader_create",
    close_record: Dispose = b"indexstore_record_reader_dispose",
    occurrences: Closure<Object> = b"indexstore_record_reader_occurrences_apply_f",
    occurrence_roles: GetBits = b"indexstore_occurrence_get_roles",
    occurrence_symbol: GetObject = b"indexstore_occurrence_get_symbol",
    position: unsafe extern "C" fn(Object, *mut c_uint, *mut c_uint) = b"indexstore_occurrence_get_line_col",
    relations: Closure<Object> = b"indexstore_occurrence_relations_apply_f",
    relation_roles: GetBits = b"indexstore_symbol_relation_get_roles",
    relation_symbol: GetObject = b"indexstore_symbol_relation_get_symbol",
    symbol_kind: GetKind = b"indexstore_symbol_get_kind",
    symbol_subkind: GetKind = b"indexstore_symbol_get_subkind",
    symbol_properties: GetBits = b"indexstore_symbol_get_properties",
    symbol_name: GetText = b"indexstore_symbol_get_name",
    symbol_usr: GetText = b"indexstore_symbol_get_usr",
}

struct Handle(Object, Dispose);
impl Handle {
    fn new(pointer: Object, dispose: Dispose) -> Result<Self, &'static str> {
        (!pointer.is_null())
            .then_some(Self(pointer, dispose))
            .ok_or(INVALID)
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) }
        }
    }
}

fn apply<T, F>(
    call: impl FnOnce(Object, Callback<T>) -> bool,
    max: usize,
    callback: F,
) -> Result<(), &'static str>
where
    F: FnMut(T) -> Result<(), &'static str>,
{
    struct Context<F> {
        callback: F,
        remaining: usize,
        fault: Option<&'static str>,
    }
    unsafe extern "C" fn invoke<T, F>(context: Object, value: T) -> bool
    where
        F: FnMut(T) -> Result<(), &'static str>,
    {
        // IndexStore's nonescaping callback uses the same typed context for this apply.
        let context = unsafe { &mut *context.cast::<Context<F>>() };
        if context.remaining == 0 {
            context.fault = Some("swift-output-budget");
            return false;
        }
        context.remaining -= 1;
        if let Err(fault) = (context.callback)(value) {
            context.fault = Some(fault);
            return false;
        }
        true
    }
    let mut state = Context {
        callback,
        remaining: max,
        fault: None,
    };
    let result = call((&mut state as *mut Context<F>).cast(), invoke::<T, F>);
    match state.fault {
        Some(fault) => Err(fault),
        None if result => Ok(()),
        None => Err(INVALID),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    pub kind: u32,
    pub subkind: u32,
    pub properties: u64,
    pub name: String,
    pub usr: String,
}
#[derive(Debug, Clone)]
pub(crate) struct Occurrence {
    pub symbol: Symbol,
    pub roles: u64,
    pub line: u32,
    pub column: u32,
    pub relations: Vec<(u64, Symbol)>,
}

fn symbol(api: &Api, handle: Object) -> Result<Symbol, &'static str> {
    if handle.is_null() {
        return Err(INVALID);
    }
    Ok(Symbol {
        kind: unsafe { (api.symbol_kind)(handle) },
        subkind: unsafe { (api.symbol_subkind)(handle) },
        properties: unsafe { (api.symbol_properties)(handle) },
        name: unsafe { (api.symbol_name)(handle) }.string()?,
        usr: unsafe { (api.symbol_usr)(handle) }.string()?,
    })
}

fn store_name(value: CText) -> Result<CString, &'static str> {
    let text = value.string()?;
    if text.is_empty() || text.contains(['/', '\\']) || matches!(text.as_str(), "." | "..") {
        return Err(INVALID);
    }
    CString::new(text).map_err(|_| INVALID)
}

fn record_binding<'a>(
    paths: &'a BTreeMap<PathBuf, String>,
    seen: &mut BTreeMap<&'a str, CString>,
    file: &Path,
    record: &CString,
) -> Result<Option<&'a str>, &'static str> {
    let unit = paths.get(file).ok_or(INVALID)?.as_str();
    match seen.get(unit) {
        Some(current) if current == record => Ok(None),
        Some(_) => Err(INVALID),
        None => {
            seen.insert(unit, record.clone());
            Ok(Some(unit))
        }
    }
}

fn checked_roles(roles: u64) -> Result<u64, &'static str> {
    (roles < 1 << 21).then_some(roles).ok_or(INVALID)
}

pub(crate) struct IndexStore(Api);

impl IndexStore {
    pub(crate) fn open(path: &Path) -> Result<Self, &'static str> {
        Ok(Self(Api::open(path)?))
    }

    pub(crate) fn read(
        &self,
        index: &Path,
        module: &str,
        paths: &BTreeMap<PathBuf, String>,
        remaining: &mut usize,
    ) -> Result<BTreeMap<String, Vec<Occurrence>>, &'static str> {
        let api = &self.0;
        let path = CString::new(index.to_str().ok_or(INVALID)?).map_err(|_| INVALID)?;
        let store = Handle::new(
            unsafe { (api.open_store)(path.as_ptr(), ptr::null_mut()) },
            api.close_store,
        )?;
        let mut records = BTreeMap::new();
        let mut seen = BTreeMap::new();
        apply(
            |context, f| unsafe { (api.units)(store.0, 1, context, f) },
            paths.len() + 128,
            |name| {
                let name = store_name(name)?;
                let unit = Handle::new(
                    unsafe { (api.open_unit)(store.0, name.as_ptr(), ptr::null_mut()) },
                    api.close_unit,
                )?;
                if unsafe { (api.system_unit)(unit.0) } {
                    return Ok(());
                }
                if unsafe { (api.module)(unit.0) }.string()? != module {
                    return Err(INVALID);
                }
                apply(
                    |context, f| unsafe { (api.deps)(unit.0, context, f) },
                    paths.len() + 1024,
                    |dep| {
                        if unsafe { (api.dep_kind)(dep) != 2 || (api.dep_system)(dep) } {
                            return Ok(());
                        }
                        let file = unsafe { (api.dep_file)(dep) }.string()?;
                        let record = store_name(unsafe { (api.dep_name)(dep) })?;
                        let Some(key) =
                            record_binding(paths, &mut seen, Path::new(&file), &record)?
                        else {
                            return Ok(());
                        };
                        let handle = Handle::new(
                            unsafe { (api.open_record)(store.0, record.as_ptr(), ptr::null_mut()) },
                            api.close_record,
                        )?;
                        let mut items = Vec::new();
                        apply(
                            |context, f| unsafe { (api.occurrences)(handle.0, context, f) },
                            *remaining,
                            |occ| {
                                let mut line = 0;
                                let mut column = 0;
                                unsafe { (api.position)(occ, &mut line, &mut column) };
                                let mut relations = Vec::new();
                                apply(
                                    |context, f| unsafe { (api.relations)(occ, context, f) },
                                    64,
                                    |rel| {
                                        relations.push((
                                            checked_roles(unsafe { (api.relation_roles)(rel) })?,
                                            symbol(api, unsafe { (api.relation_symbol)(rel) })?,
                                        ));
                                        Ok(())
                                    },
                                )?;
                                items.push(Occurrence {
                                    symbol: symbol(api, unsafe { (api.occurrence_symbol)(occ) })?,
                                    roles: checked_roles(unsafe { (api.occurrence_roles)(occ) })?,
                                    line,
                                    column,
                                    relations,
                                });
                                Ok(())
                            },
                        )?;
                        *remaining -= items.len();
                        records.insert(key.to_owned(), items);
                        Ok(())
                    },
                )
            },
        )?;
        Ok(records)
    }
}

pub(crate) fn occurrence_budget() -> usize {
    MAX_OCCURRENCES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_indexstore_refuses_unbounded_or_malformed_strings() {
        let make = |bytes: &[u8]| {
            CText {
                data: bytes.as_ptr(),
                length: bytes.len(),
            }
            .string()
        };
        assert_eq!(make(b"valid"), Ok("valid".into()));
        assert_eq!(make(b"bad\0value"), Err(INVALID));
        assert_eq!(make(&[0xff]), Err(INVALID));
        assert_eq!(make(&[b'a'; 4097]), Err(INVALID));
    }

    #[test]
    fn swift_indexstore_rejects_unbound_paths_and_conflicting_records() {
        let path = PathBuf::from("capture/Sources/Example.swift");
        let paths = BTreeMap::from([(path.clone(), "Sources/Example.swift".to_owned())]);
        let mut seen = BTreeMap::new();
        let record = CString::new("Example.swift-AA").unwrap();
        let conflict = CString::new("Example.swift-BB").unwrap();
        assert_eq!(
            record_binding(&paths, &mut seen, &path, &record),
            Ok(Some("Sources/Example.swift"))
        );
        assert_eq!(record_binding(&paths, &mut seen, &path, &record), Ok(None));
        assert_eq!(
            record_binding(&paths, &mut seen, &path, &conflict),
            Err(INVALID)
        );
        assert_eq!(
            record_binding(&paths, &mut seen, Path::new("another.swift"), &record),
            Err(INVALID)
        );
        assert_eq!(checked_roles(1 << 20), Ok(1 << 20));
        assert_eq!(checked_roles(1 << 21), Err(INVALID));
    }
}
