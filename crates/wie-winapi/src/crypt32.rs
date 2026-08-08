//! Handles `CRYPT32.dll` — real cryptography via host entropy + hashing.
//!
//! Provider/hash handles are fake `u64`s from a private namespace; the hash
//! values themselves are computed for real (SHA-1 / SHA-256 via the `sha1` /
//! `sha2` crates) and `CryptGenRandom` draws from `/dev/urandom`.
//! `Crypt32State` owns the handle tables and lives in a `DllStateMap` slot,
//! heap-allocated on first load.

use std::fs::File;
use std::io::Read;

use ahash::{HashMap, HashMapExt};
use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::{HandlerContext, WinApiHandlerResult};

/// `CALG_SHA1` — SHA-1 hash algorithm id.
const CALG_SHA1: u64 = 0x8004;
/// `CALG_SHA_256` — SHA-256 hash algorithm id.
const CALG_SHA_256: u64 = 0x800c;
/// `HP_HASHVAL` — `CryptGetHashParam` parameter returning the hash value.
const HP_HASHVAL: u64 = 0x0002;
/// `NTE_BAD_ALGID` — the requested algorithm is not supported.
const NTE_BAD_ALGID: u32 = 0x8009_0003;
/// `NTE_BAD_HASH` — the hash handle is not valid.
const NTE_BAD_HASH: u32 = 0x8009_0002;
/// `NTE_BAD_PARAM` — the parameter id is not valid.
const NTE_BAD_PARAM: u32 = 0x8009_0006;
/// First fake crypto handle, above the other handle namespaces in WIE.
const FIRST_CRYPTO_HANDLE: u64 = 0x5100_0000;

/// Hash algorithms `CryptCreateHash` can instantiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashAlgorithm {
    Sha1,
    Sha256,
}

/// A guest-visible hash object: algorithm + all data fed via `CryptHashData`.
#[derive(Debug)]
struct HashCtx {
    algorithm: HashAlgorithm,
    data: Vec<u8>,
}

/// Crypt provider / hash handle state, owned by this module.
#[derive(Debug)]
pub struct Crypt32State {
    /// Fake `HCRYPTPROV` handles handed out by `CryptAcquireContext`.
    providers: HashMap<u64, ()>,
    /// Fake `HCRYPTHASH` handles → hash context.
    hashes: HashMap<u64, HashCtx>,
    /// Next handle value in the private crypto namespace.
    next_handle: u64,
}

impl Default for Crypt32State {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            hashes: HashMap::new(),
            next_handle: FIRST_CRYPTO_HANDLE,
        }
    }
}

impl Crypt32State {
    /// Allocate a fresh fake provider handle.
    fn alloc_provider(&mut self) -> u64 {
        self.next_handle += 1;
        let handle = self.next_handle;
        self.providers.insert(handle, ());
        handle
    }

    /// Allocate a fresh fake hash handle for `algorithm`.
    fn alloc_hash(&mut self, algorithm: HashAlgorithm) -> u64 {
        self.next_handle += 1;
        let handle = self.next_handle;
        self.hashes.insert(
            handle,
            HashCtx {
                algorithm,
                data: Vec::new(),
            },
        );
        handle
    }
}

/// Dispatch a `CRYPT32.dll` export by name (case-insensitive).
pub fn dispatch_crypt32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "cryptacquirecontexta" => Ok(Some(handle_crypt_acquire_context(ctx)?)),
        "cryptacquirecontextw" => Ok(Some(handle_crypt_acquire_context(ctx)?)),
        "cryptreleasecontext" => Ok(Some(handle_crypt_release_context(ctx)?)),
        "cryptgenrandom" => Ok(Some(handle_crypt_gen_random(ctx)?)),
        "cryptcreatehash" => Ok(Some(handle_crypt_create_hash(ctx)?)),
        "cryptdestroyhash" => Ok(Some(handle_crypt_destroy_hash(ctx)?)),
        "crypthashdata" => Ok(Some(handle_crypt_hash_data(ctx)?)),
        "cryptgethashparam" => Ok(Some(handle_crypt_get_hash_param(ctx)?)),
        _ => Ok(None),
    }
}

/// `BOOL CryptAcquireContextA/W(HCRYPTPROV *phProv, LPCSTR/LPCWSTR szContainer,
/// LPCSTR/LPCWSTR szProvider, DWORD dwProvType, DWORD dwFlags)`
///
/// Both wide and ANSI forms share this body: the container/provider strings are
/// ignored and a fresh fake provider handle is written to `*phProv`.
fn handle_crypt_acquire_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ph_prov = engine.read_rcx()?;
    let _container = engine.read_rdx()?;
    let _provider = engine.read_r8()?;
    let _prov_type = engine.read_r9()?;
    let _flags = read_stack_arg5(engine)?;
    let handle = ctx.state.crypt32().alloc_provider();
    engine.mem_write(ph_prov, &handle.to_le_bytes())?;
    ctx.finish(1)
}

/// `BOOL CryptReleaseContext(HCRYPTPROV hProv, DWORD dwFlags)`
fn handle_crypt_release_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let h_prov = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    ctx.state.crypt32().providers.remove(&h_prov);
    ctx.finish(1)
}

/// `BOOL CryptGenRandom(HCRYPTPROV hProv, DWORD dwLen, BYTE *pbBuffer)`
///
/// Fills `pbBuffer` with `dwLen` bytes of real entropy from `/dev/urandom`.
fn handle_crypt_gen_random(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _h_prov = engine.read_rcx()?;
    let dw_len_raw = engine.read_rdx()?;
    let pb_buffer = engine.read_r8()?;
    let dw_len = u32::try_from(dw_len_raw & u64::from(u32::MAX)).context("CryptGenRandom dwLen")?;
    let len = usize::try_from(dw_len)?;
    if len > 0 {
        let mut entropy = vec![0_u8; len];
        fill_entropy(&mut entropy)?;
        engine.mem_write(pb_buffer, &entropy)?;
    }
    ctx.finish(1)
}

/// `BOOL CryptCreateHash(HCRYPTPROV hProv, ALG_ID Algid, HCRYPTKEY hKey,
/// DWORD dwFlags, HCRYPTHASH *phHash)`
///
/// Maps `Algid` to a real hash algorithm; unknown ids fail with `NTE_BAD_ALGID`.
fn handle_crypt_create_hash(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _h_prov = engine.read_rcx()?;
    let algid = engine.read_rdx()?;
    let _h_key = engine.read_r8()?;
    let _flags = engine.read_r9()?;
    let ph_hash = read_stack_arg5(engine)?;

    let algorithm = match algid {
        CALG_SHA1 => HashAlgorithm::Sha1,
        CALG_SHA_256 => HashAlgorithm::Sha256,
        _ => {
            ctx.state.process.last_error = NTE_BAD_ALGID;
            return ctx.finish(0);
        }
    };

    let handle = ctx.state.crypt32().alloc_hash(algorithm);
    engine.mem_write(ph_hash, &handle.to_le_bytes())?;
    ctx.finish(1)
}

/// `BOOL CryptDestroyHash(HCRYPTHASH hHash)`
fn handle_crypt_destroy_hash(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let h_hash = engine.read_rcx()?;
    ctx.state.crypt32().hashes.remove(&h_hash);
    ctx.finish(1)
}

/// `BOOL CryptHashData(HCRYPTHASH hHash, const BYTE *pbData, DWORD dwDataLen,
/// DWORD dwFlags)`
///
/// Appends the guest bytes to the hash context's accumulated data.
fn handle_crypt_hash_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let h_hash = engine.read_rcx()?;
    let pb_data = engine.read_rdx()?;
    let dw_len_raw = engine.read_r8()?;
    let _flags = engine.read_r9()?;
    let dw_len =
        u32::try_from(dw_len_raw & u64::from(u32::MAX)).context("CryptHashData dwDataLen")?;
    let len = usize::try_from(dw_len)?;

    if len > 0 {
        let mut chunk = vec![0_u8; len];
        engine.mem_read(pb_data, &mut chunk)?;
        let found = {
            let state = ctx.state.crypt32();
            if let Some(hash_ctx) = state.hashes.get_mut(&h_hash) {
                hash_ctx.data.extend_from_slice(&chunk);
                true
            } else {
                false
            }
        };
        if !found {
            ctx.state.process.last_error = NTE_BAD_HASH;
            return ctx.finish(0);
        }
    }
    ctx.finish(1)
}

/// `BOOL CryptGetHashParam(HCRYPTHASH hHash, DWORD dwParam, BYTE *pbData,
/// DWORD *pdwDataLen, DWORD dwFlags)`
///
/// `HP_HASHVAL` computes the digest on demand, copies
/// `min(digest_len, *pdwDataLen)` bytes into `pbData` (skipped when `pbData`
/// is `NULL` — the length probe), and reports the full digest length in
/// `*pdwDataLen`.
fn handle_crypt_get_hash_param(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let h_hash = engine.read_rcx()?;
    let dw_param = engine.read_rdx()?;
    let pb_data = engine.read_r8()?;
    let pdw_data_len = engine.read_r9()?;
    let _flags = read_stack_arg5(engine)?;

    if dw_param != HP_HASHVAL {
        ctx.state.process.last_error = NTE_BAD_PARAM;
        return ctx.finish(0);
    }

    // Caller-supplied buffer size, read before touching the hash table.
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(pdw_data_len, &mut len_bytes)?;
    let caller_len = u32::from_le_bytes(len_bytes);

    let digest: Option<Vec<u8>> = ctx
        .state
        .crypt32()
        .hashes
        .get(&h_hash)
        .map(compute_digest)
        .transpose()?;
    let Some(digest) = digest else {
        ctx.state.process.last_error = NTE_BAD_HASH;
        return ctx.finish(0);
    };

    let digest_len = digest.len();
    if pb_data != 0 {
        let copy_len = digest_len.min(usize::try_from(caller_len)?);
        if let Some(to_copy) = digest.get(..copy_len) {
            engine.mem_write(pb_data, to_copy)?;
        }
    }
    let full_len = u32::try_from(digest_len).context("digest length exceeds a DWORD")?;
    engine.mem_write(pdw_data_len, &full_len.to_le_bytes())?;
    ctx.finish(1)
}

/// Compute the real digest of a hash context's accumulated data.
fn compute_digest(hash_ctx: &HashCtx) -> Result<Vec<u8>> {
    let digest = match hash_ctx.algorithm {
        HashAlgorithm::Sha1 => Sha1::digest(&hash_ctx.data).to_vec(),
        HashAlgorithm::Sha256 => Sha256::digest(&hash_ctx.data).to_vec(),
    };
    Ok(digest)
}

/// Fill `buf` with bytes from the host CSPRNG (`/dev/urandom`).
fn fill_entropy(buf: &mut [u8]) -> Result<()> {
    let mut urandom = File::open("/dev/urandom").context("open /dev/urandom")?;
    urandom.read_exact(buf).context("read /dev/urandom")?;
    Ok(())
}

/// Read the 5th Win64 argument from the stack.
///
/// At handler entry `[RSP]` holds the return address and the caller's 0x20
/// bytes of shadow space follow it, so the 5th parameter sits at `RSP + 0x28`.
fn read_stack_arg5(engine: &mut dyn wie_cpu::CpuEngine) -> Result<u64> {
    let rsp = engine.read_rsp()?;
    let mut bytes = [0_u8; 8];
    engine.mem_read(rsp.wrapping_add(0x28), &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
