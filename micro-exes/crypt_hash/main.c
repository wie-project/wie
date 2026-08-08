/*
 * Micro-PE CRYPT32.1: real SHA-1 / SHA-256 hashing + host entropy.
 *
 * Freestanding PE64. Drives the host crypt32 handlers end to end:
 * acquire a provider, draw 16 bytes of entropy, hash "abc" with SHA-1
 * and SHA-256, and compare against the published test vectors.
 *
 * Exit codes:
 *   0 — ok
 *   1 — CryptAcquireContextW failed
 *   2 — CryptGenRandom failed or returned all-zero bytes
 *   3 — CryptCreateHash (SHA-1) failed
 *   4 — CryptHashData (SHA-1) failed
 *   5 — SHA-1 digest mismatch (or bad HP_HASHVAL length)
 *   6 — SHA-256 round-trip failed
 */

#include <windows.h>
#include <wincrypt.h>

/* SHA-1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d */
static const BYTE SHA1_ABC[20] = {
    0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e,
    0x25, 0x71, 0x78, 0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
};

/* SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad */
static const BYTE SHA256_ABC[32] = {
    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde,
    0x5d, 0xae, 0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c,
    0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
};

static int memeq(const BYTE *a, const BYTE *b, DWORD n) {
    DWORD i;
    for (i = 0; i < n; i++) {
        if (a[i] != b[i]) {
            return 0;
        }
    }
    return 1;
}

static int all_zero(const BYTE *buf, DWORD n) {
    DWORD i;
    for (i = 0; i < n; i++) {
        if (buf[i] != 0) {
            return 0;
        }
    }
    return 1;
}

/* Full create/hash/getparam/verify round-trip; 1 on success, 0 on any failure. */
static int hash_round_trip(HCRYPTPROV h_prov, ALG_ID algid,
                           const BYTE *expected, DWORD expected_len) {
    HCRYPTHASH h_hash = 0;
    BYTE digest[64];
    DWORD len = sizeof(digest);

    if (!CryptCreateHash(h_prov, algid, 0, 0, &h_hash)) {
        return 0;
    }
    if (!CryptHashData(h_hash, (const BYTE *)"abc", 3, 0)) {
        CryptDestroyHash(h_hash);
        return 0;
    }
    if (!CryptGetHashParam(h_hash, HP_HASHVAL, digest, &len, 0)) {
        CryptDestroyHash(h_hash);
        return 0;
    }
    if (len != expected_len) {
        CryptDestroyHash(h_hash);
        return 0;
    }
    if (!memeq(digest, expected, expected_len)) {
        CryptDestroyHash(h_hash);
        return 0;
    }
    CryptDestroyHash(h_hash);
    return 1;
}

void entry(void) {
    HCRYPTPROV h_prov = 0;
    HCRYPTHASH h_hash = 0;
    BYTE rand_buf[16];
    BYTE digest[64];
    DWORD len;

    /* 1: acquire a provider (strings NULL → default provider). */
    if (!CryptAcquireContextW(&h_prov, NULL, NULL, PROV_RSA_FULL, 0)) {
        ExitProcess(1);
    }

    /* 2: 16 bytes of real entropy, must not be all zero. */
    if (!CryptGenRandom(h_prov, sizeof(rand_buf), rand_buf)) {
        ExitProcess(2);
    }
    if (all_zero(rand_buf, sizeof(rand_buf))) {
        ExitProcess(2);
    }

    /* 3–5: SHA-1("abc"). */
    if (!CryptCreateHash(h_prov, CALG_SHA1, 0, 0, &h_hash)) {
        ExitProcess(3);
    }
    if (!CryptHashData(h_hash, (const BYTE *)"abc", 3, 0)) {
        ExitProcess(4);
    }
    len = sizeof(digest);
    if (!CryptGetHashParam(h_hash, HP_HASHVAL, digest, &len, 0)) {
        ExitProcess(5);
    }
    if (len != 20 || !memeq(digest, SHA1_ABC, 20)) {
        ExitProcess(5);
    }
    if (!CryptDestroyHash(h_hash)) {
        ExitProcess(5);
    }

    /* 6: SHA-256("abc"). */
    if (!hash_round_trip(h_prov, CALG_SHA_256, SHA256_ABC, 32)) {
        ExitProcess(6);
    }

    CryptReleaseContext(h_prov, 0);
    ExitProcess(0);
}
