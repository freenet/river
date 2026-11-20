#!/usr/bin/env python3
"""Regenerates the frozen direct-message interop vectors in src/rooms/dm_tests.rs
(PYTHON_RAW, PYTHON_CBOR).

An independent implementation of River's documented DM envelope, not river-core:
  ephemeral X25519 key, shared secret = X25519(eph, recipient), key = SHA-256(shared),
  AES-256-GCM, wire = ephemeral_pub(32) || nonce(12) || ciphertext+tag.
The recipient's X25519 key is the clamped SHA-512 of its Ed25519 seed (here seed = [2; 32],
the test's `ME`). The CBOR body is hand-encoded: 0x80 then {"Text": {"text": "..."}}.

Needs the `cryptography` package (tested with 41.0.7). Run: python3 make_dm_interop_vectors.py
"""
import hashlib
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


def clamp(seed):
    h = bytearray(hashlib.sha512(seed).digest()[:32])
    h[0] &= 248
    h[31] &= 127
    h[31] |= 64
    return bytes(h)


recipient_pub = X25519PrivateKey.from_private_bytes(clamp(bytes([2]) * 32)).public_key()


def seal(eph_seed, nonce, plaintext):
    eph = X25519PrivateKey.from_private_bytes(eph_seed)
    key = hashlib.sha256(eph.exchange(recipient_pub)).digest()
    ct = AESGCM(key).encrypt(nonce, plaintext, None)
    pub = eph.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    return (pub + nonce + ct).hex()


raw = b"hello from an independent implementation"
text = b"structured hello"
cbor = b"\x80" + b"\xa1\x64Text\xa1\x64text" + bytes([0x60 + len(text)]) + text
print("PYTHON_RAW =", seal(bytes([0x11]) * 32, bytes([0x21]) * 12, raw))
print("PYTHON_CBOR =", seal(bytes([0x12]) * 32, bytes([0x22]) * 12, cbor))
