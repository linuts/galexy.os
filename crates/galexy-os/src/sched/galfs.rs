//! galfs: owner-qualified paths and tokens.
//!
//! Each actor has one root directory. `/Desktop` is that actor's child
//! named Desktop. `/dan@Desktop` is dan's. A token names an object and a
//! set of rights; the path is only a lookup. Lock order: [`THREADS`]
//! then this table.

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;

use galexy_abi::SysError;

/// Objects the kernel will hold (files, directories, and actor roots).
pub const OBJECT_SLOTS: usize = 32;
/// Actors (users) the table can name.
const ACTOR_SLOTS: usize = 8;
/// Bytes one file can hold.
pub const FILE_BYTES: usize = 256;
/// Tokens one task may hold.
pub const TOKEN_SLOTS: usize = 8;
/// Path components after the optional owner.
pub const MAX_DEPTH: usize = 8;
/// One path-component name.
const NAME_CAP: usize = 64;
/// Actor name length.
const ACTOR_NAME: usize = 32;

const KIND_EMPTY: u8 = 0;
pub(crate) const KIND_FILE: u8 = 1;
pub(crate) const KIND_DIR: u8 = 2;

/// Parent of an actor root.
const NO_PARENT: u16 = 0xffff;
/// No object / empty token.
pub const NO_OBJECT: u16 = 0xffff;

pub const RIGHT_READ: u8 = 1;
pub const RIGHT_WRITE: u8 = 2;
pub const RIGHT_LIST: u8 = 4;
pub const RIGHT_CREATE: u8 = 8;
pub const RIGHT_REMOVE: u8 = 16;
pub const RIGHT_ALL: u8 = RIGHT_READ | RIGHT_WRITE | RIGHT_LIST | RIGHT_CREATE | RIGHT_REMOVE;

#[derive(Clone, Copy)]
pub struct Token {
    pub object: u16,
    pub rights: u8,
}

impl Token {
    pub const fn empty() -> Self {
        Self {
            object: NO_OBJECT,
            rights: 0,
        }
    }

    pub const fn is_live(self) -> bool {
        self.object != NO_OBJECT
    }
}

/// Credentials copied onto a task at spawn.
#[derive(Clone, Copy)]
pub struct FsCred {
    pub root: u16,
    pub tokens: [Token; TOKEN_SLOTS],
}

impl FsCred {
    pub const fn none() -> Self {
        Self {
            root: NO_OBJECT,
            tokens: [Token::empty(); TOKEN_SLOTS],
        }
    }

    /// Full rights on `root`. Used for the interactive shell and test blobs.
    pub fn launcher(root: u16) -> Self {
        let mut tokens = [Token::empty(); TOKEN_SLOTS];
        tokens[0] = Token {
            object: root,
            rights: RIGHT_ALL,
        };
        Self { root, tokens }
    }
}

#[derive(Clone, Copy)]
struct Actor {
    used: bool,
    name: [u8; ACTOR_NAME],
    name_len: u8,
    root: u16,
}

impl Actor {
    const fn empty() -> Self {
        Self {
            used: false,
            name: [0; ACTOR_NAME],
            name_len: 0,
            root: NO_OBJECT,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Object {
    pub(crate) kind: u8,
    parent: u16,
    /// Actor that owns this object (its root's actor).
    actor: u8,
    name: [u8; NAME_CAP],
    name_len: u8,
    pub(crate) data: [u8; FILE_BYTES],
    pub(crate) len: u16,
}

impl Object {
    const fn empty() -> Self {
        Self {
            kind: KIND_EMPTY,
            parent: NO_PARENT,
            actor: 0,
            name: [0; NAME_CAP],
            name_len: 0,
            data: [0; FILE_BYTES],
            len: 0,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.kind != KIND_EMPTY && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

struct Table {
    actors: [Actor; ACTOR_SLOTS],
    objects: [Object; OBJECT_SLOTS],
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    actors: [Actor::empty(); ACTOR_SLOTS],
    objects: [Object::empty(); OBJECT_SLOTS],
});

static BOOTED: AtomicBool = AtomicBool::new(false);

/// Alex's root object. Valid after [`init`].
static ALEX_ROOT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(NO_OBJECT);

/// Builds actor `alex` and its empty root. Call once from scheduler init.
pub fn init() {
    if BOOTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut table = TABLE.lock();
    let root = add_actor(&mut table, "alex").expect("galfs: alex");
    ALEX_ROOT.store(root, Ordering::Relaxed);
}

/// Credentials for the default boot actor.
pub fn alex_cred() -> FsCred {
    let root = ALEX_ROOT.load(Ordering::Relaxed);
    debug_assert!(root != NO_OBJECT, "galfs: init before alex_cred");
    FsCred::launcher(root)
}

/// Adds an actor and an empty root. Test and boot only.
pub fn add_actor_named(name: &str) -> Result<u16, SysError> {
    let mut table = TABLE.lock();
    add_actor(&mut table, name)
}

fn add_actor(table: &mut Table, name: &str) -> Result<u16, SysError> {
    if !component_ok(name) || name.len() > ACTOR_NAME {
        return Err(SysError::BadValue);
    }
    if table.actors.iter().any(|a| a.name_is(name)) {
        return Err(SysError::Unsupported);
    }
    let Some(ai) = table.actors.iter().position(|a| !a.used) else {
        return Err(SysError::NoResource);
    };
    let Some(oi) = free_object(table) else {
        return Err(SysError::NoResource);
    };
    let actor = &mut table.actors[ai];
    actor.used = true;
    actor.name = [0; ACTOR_NAME];
    actor.name[..name.len()].copy_from_slice(name.as_bytes());
    actor.name_len = name.len() as u8;
    actor.root = oi as u16;
    let obj = &mut table.objects[oi];
    *obj = Object::empty();
    obj.kind = KIND_DIR;
    obj.parent = NO_PARENT;
    obj.actor = ai as u8;
    // Root has an empty name; paths start at its children.
    Ok(oi as u16)
}

/// Creates a directory under an actor's root. Test helper.
pub fn mkdir_under_root(root: u16, name: &str) -> Result<u16, SysError> {
    if !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    if root as usize >= OBJECT_SLOTS || table.objects[root as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    if find_child(&table, root, name).is_some() {
        return Err(SysError::Unsupported);
    }
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let actor = table.objects[root as usize].actor;
    let obj = &mut table.objects[oi];
    obj.kind = KIND_DIR;
    obj.parent = root;
    obj.actor = actor;
    place_name(obj, name);
    Ok(oi as u16)
}

/// Creates an empty file under `parent`. Test helper.
pub fn create_file_under(parent: u16, name: &str) -> Result<u16, SysError> {
    if !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    if parent as usize >= OBJECT_SLOTS || table.objects[parent as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    if find_child(&table, parent, name).is_some() {
        return Err(SysError::Unsupported);
    }
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let actor = table.objects[parent as usize].actor;
    let obj = &mut table.objects[oi];
    obj.kind = KIND_FILE;
    obj.parent = parent;
    obj.actor = actor;
    place_name(obj, name);
    obj.data = [0; FILE_BYTES];
    obj.len = 0;
    Ok(oi as u16)
}

fn free_object(table: &Table) -> Option<usize> {
    table.objects.iter().position(|o| o.kind == KIND_EMPTY)
}

fn component_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= NAME_CAP
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

fn place_name(obj: &mut Object, name: &str) {
    obj.name = [0; NAME_CAP];
    obj.name[..name.len()].copy_from_slice(name.as_bytes());
    obj.name_len = name.len() as u8;
}

/// A path after the optional leading `/` and `owner@` on the first component.
pub(crate) struct ParsedPath<'a> {
    pub(crate) owner: Option<&'a str>,
    pub(crate) comps: [&'a str; MAX_DEPTH],
    pub(crate) n: usize,
    pub(crate) dir: bool,
}

/// Splits `name`. A trailing slash marks a directory. The first component
/// may be `owner@leaf`. `.` and `..` are rejected.
pub(crate) fn parse_path(name: &str) -> Result<ParsedPath<'_>, SysError> {
    let name = name.strip_prefix('/').unwrap_or(name);
    let (body, dir) = if let Some(stripped) = name.strip_suffix('/') {
        if stripped.is_empty() || stripped.ends_with('/') {
            return Err(SysError::BadValue);
        }
        (stripped, true)
    } else {
        (name, false)
    };
    if body.is_empty() {
        return Err(SysError::BadValue);
    }
    let mut comps = [""; MAX_DEPTH];
    let mut n = 0usize;
    let mut owner = None;
    for (i, comp) in body.split('/').enumerate() {
        if comp.is_empty() || n >= MAX_DEPTH {
            return Err(SysError::BadValue);
        }
        if i == 0 {
            if let Some((own, leaf)) = comp.split_once('@') {
                if own.is_empty() || leaf.is_empty() || leaf.contains('@') {
                    return Err(SysError::BadValue);
                }
                if !component_ok(own) || !component_ok(leaf) {
                    return Err(SysError::BadValue);
                }
                owner = Some(own);
                comps[n] = leaf;
                n += 1;
                continue;
            }
        } else if comp.contains('@') {
            return Err(SysError::BadValue);
        }
        if !component_ok(comp) {
            return Err(SysError::BadValue);
        }
        comps[n] = comp;
        n += 1;
    }
    if n == 0 {
        return Err(SysError::BadValue);
    }
    Ok(ParsedPath {
        owner,
        comps,
        n,
        dir,
    })
}

fn find_actor_root(table: &Table, name: &str) -> Result<u16, SysError> {
    table
        .actors
        .iter()
        .find(|a| a.name_is(name))
        .map(|a| a.root)
        .ok_or(SysError::NotFound)
}

fn find_child(table: &Table, parent: u16, name: &str) -> Option<usize> {
    table
        .objects
        .iter()
        .position(|o| o.kind != KIND_EMPTY && o.parent == parent && o.name_is(name))
}

fn has_child(table: &Table, parent: u16) -> bool {
    table
        .objects
        .iter()
        .any(|o| o.kind != KIND_EMPTY && o.parent == parent)
}

/// Walks every component except the last under `start`.
fn walk_parents(table: &Table, start: u16, parsed: &ParsedPath<'_>) -> Result<u16, SysError> {
    let mut parent = start;
    for comp in &parsed.comps[..parsed.n.saturating_sub(1)] {
        let Some(index) = find_child(table, parent, comp) else {
            return Err(SysError::NotFound);
        };
        if table.objects[index].kind != KIND_DIR {
            return Err(SysError::NotFound);
        }
        parent = index as u16;
    }
    Ok(parent)
}

fn start_root(table: &Table, cred: &FsCred, parsed: &ParsedPath<'_>) -> Result<u16, SysError> {
    match parsed.owner {
        Some(owner) => find_actor_root(table, owner),
        None => {
            if cred.root == NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            Ok(cred.root)
        }
    }
}

/// True when `ancestor` is `object` or a parent of it.
fn covers_object(table: &Table, ancestor: u16, object: u16) -> bool {
    let mut cur = object;
    for _ in 0..OBJECT_SLOTS {
        if cur == ancestor {
            return true;
        }
        if cur as usize >= OBJECT_SLOTS {
            return false;
        }
        let parent = table.objects[cur as usize].parent;
        if parent == NO_PARENT {
            return cur == ancestor;
        }
        cur = parent;
    }
    false
}

/// Whether `cred` holds `need` on `object` or an ancestor.
fn token_allows(table: &Table, cred: &FsCred, object: u16, need: u8) -> bool {
    for token in &cred.tokens {
        if !token.is_live() || token.rights & need != need {
            continue;
        }
        if covers_object(table, token.object, object) {
            return true;
        }
    }
    false
}

fn cred_from_tokens(root: u16, tokens: &[Token; TOKEN_SLOTS]) -> FsCred {
    FsCred {
        root,
        tokens: *tokens,
    }
}

/// Resolves `parsed` to an object index, checking existence only.
fn lookup(table: &Table, cred: &FsCred, parsed: &ParsedPath<'_>) -> Result<(u16, usize), SysError> {
    let start = start_root(table, cred, parsed)?;
    let parent = walk_parents(table, start, parsed)?;
    let last = parsed.comps[parsed.n - 1];
    let Some(found) = find_child(table, parent, last) else {
        return Err(SysError::NotFound);
    };
    Ok((parent, found))
}

/// Looks up a file for open. Directory → `Unsupported` after the token check.
pub(crate) fn open_file(
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u16, SysError> {
    let parsed = parse_path(name)?;
    if parsed.dir {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u16;
    if !token_allows(&table, &cred, index, RIGHT_READ) {
        return Err(SysError::AccessDenied);
    }
    if table.objects[found].kind != KIND_FILE {
        return Err(SysError::Unsupported);
    }
    Ok(index)
}

/// Creates a file or directory. `replace` empties an existing file.
pub(crate) fn create(
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    replace: bool,
) -> Result<Option<u16>, SysError> {
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let mut table = TABLE.lock();
    let start = start_root(&table, &cred, &parsed)?;
    let parent = walk_parents(&table, start, &parsed)?;
    if !token_allows(&table, &cred, parent, RIGHT_CREATE) {
        return Err(SysError::AccessDenied);
    }
    let last = parsed.comps[parsed.n - 1];
    if let Some(found) = find_child(&table, parent, last) {
        if replace && !parsed.dir && table.objects[found].kind == KIND_FILE {
            let index = found as u16;
            if !token_allows(&table, &cred, index, RIGHT_WRITE) {
                return Err(SysError::AccessDenied);
            }
            table.objects[found].data = [0; FILE_BYTES];
            table.objects[found].len = 0;
            return Ok(Some(index));
        }
        return Err(SysError::Unsupported);
    }
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let actor = table.objects[parent as usize].actor;
    let obj = &mut table.objects[oi];
    obj.kind = if parsed.dir { KIND_DIR } else { KIND_FILE };
    obj.parent = parent;
    obj.actor = actor;
    place_name(obj, last);
    obj.data = [0; FILE_BYTES];
    obj.len = 0;
    if parsed.dir {
        Ok(None)
    } else {
        Ok(Some(oi as u16))
    }
}

/// Removes a file or empty directory.
pub(crate) fn remove(
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u16, SysError> {
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let mut table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u16;
    if !token_allows(&table, &cred, index, RIGHT_REMOVE) {
        return Err(SysError::AccessDenied);
    }
    let kind = table.objects[found].kind;
    if parsed.dir && kind != KIND_DIR {
        return Err(SysError::Unsupported);
    }
    if kind == KIND_DIR && has_child(&table, index) {
        return Err(SysError::Unsupported);
    }
    // Never remove an actor root.
    if table.objects[found].parent == NO_PARENT {
        return Err(SysError::Unsupported);
    }
    table.objects[found] = Object::empty();
    Ok(index)
}

/// Reads file bytes under the global lock.
pub(crate) fn with_file<R>(index: u16, f: impl FnOnce(&Object) -> R) -> Option<R> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(f(&table.objects[i]))
}

/// Writes file bytes under the global lock.
pub(crate) fn with_file_mut<R>(index: u16, f: impl FnOnce(&mut Object) -> R) -> Option<R> {
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(f(&mut table.objects[i]))
}

/// Appends `src` to a file. Returns the byte count written.
pub(crate) fn append(index: u16, src: &[u8]) -> Option<usize> {
    with_file_mut(index, |stored| {
        let start = stored.len as usize;
        let n = src.len().min(FILE_BYTES.saturating_sub(start));
        stored.data[start..start + n].copy_from_slice(&src[..n]);
        stored.len = (start + n) as u16;
        n
    })
}

fn path_bytes(table: &Table, index: usize, viewer_root: u16, out: &mut [u8]) -> Option<usize> {
    let mut chain = [0usize; MAX_DEPTH];
    let mut depth = 0usize;
    let mut cur = index;
    loop {
        if depth >= MAX_DEPTH {
            return None;
        }
        let obj = &table.objects[cur];
        if obj.kind == KIND_EMPTY {
            return None;
        }
        if obj.parent == NO_PARENT {
            // Stop before the root; children form the path.
            break;
        }
        chain[depth] = cur;
        depth += 1;
        cur = obj.parent as usize;
        if cur >= OBJECT_SLOTS {
            return None;
        }
    }
    if depth == 0 {
        return None;
    }
    let first = chain[depth - 1];
    let mut walk = first;
    let object_root = loop {
        let p = table.objects[walk].parent;
        if p == NO_PARENT {
            break walk as u16;
        }
        walk = p as usize;
        if walk >= OBJECT_SLOTS {
            return None;
        }
    };
    let foreign = object_root != viewer_root;

    let mut len = 0usize;
    for (i, slot) in chain[..depth].iter().rev().enumerate() {
        let file = &table.objects[*slot];
        if i == 0 && foreign {
            let actor = &table.actors[file.actor as usize];
            let an = actor.name_len as usize;
            if len + an + 1 + file.name_len as usize > out.len() {
                return None;
            }
            out[len..len + an].copy_from_slice(&actor.name[..an]);
            len += an;
            out[len] = b'@';
            len += 1;
        } else if len > 0 {
            if len >= out.len() {
                return None;
            }
            out[len] = b'/';
            len += 1;
        }
        let name_len = file.name_len as usize;
        if len + name_len > out.len() {
            return None;
        }
        out[len..len + name_len].copy_from_slice(&file.name[..name_len]);
        len += name_len;
    }
    if table.objects[index].kind == KIND_DIR {
        if len >= out.len() {
            return None;
        }
        out[len] = b'/';
        len += 1;
    }
    Some(len)
}

/// Calls `each` with every path the credentials may list.
pub fn for_each_visible(root: u16, tokens: &[Token; TOKEN_SLOTS], mut each: impl FnMut(&[u8])) {
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    for index in 0..OBJECT_SLOTS {
        let obj = &table.objects[index];
        if obj.kind == KIND_EMPTY || obj.parent == NO_PARENT {
            continue;
        }
        // LIST on this object or an ancestor (typically the parent).
        if !token_allows(&table, &cred, index as u16, RIGHT_LIST)
            && !token_allows(&table, &cred, obj.parent, RIGHT_LIST)
        {
            continue;
        }
        let mut buf = [0u8; NAME_CAP];
        let Some(n) = path_bytes(&table, index, root, &mut buf) else {
            continue;
        };
        each(&buf[..n]);
    }
}

/// Resolves `name` and checks that `cred` holds every bit in `rights`.
/// Returns the object index. Used by the `grant` syscall.
pub(crate) fn resolve_and_check(
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    rights: u8,
) -> Result<u16, SysError> {
    if rights == 0 {
        return Err(SysError::BadValue);
    }
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u16;
    if !token_allows(&table, &cred, index, rights) {
        return Err(SysError::AccessDenied);
    }
    Ok(index)
}

/// Installs a token into a fixed slot array. Same object merges rights.
pub fn push_token(
    tokens: &mut [Token; TOKEN_SLOTS],
    object: u16,
    rights: u8,
) -> Result<(), SysError> {
    if object == NO_OBJECT || rights == 0 {
        return Err(SysError::BadValue);
    }
    if let Some(slot) = tokens.iter_mut().find(|t| t.object == object) {
        slot.rights |= rights;
        return Ok(());
    }
    if let Some(slot) = tokens.iter_mut().find(|t| !t.is_live()) {
        *slot = Token { object, rights };
        Ok(())
    } else {
        Err(SysError::NoResource)
    }
}
