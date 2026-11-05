# Android wireless ADB dependencies

Octendra 0.12.8 uses the pinned LibADB Android 3.1.1 artifact for the ADB transport, local mDNS discovery, Android public-key encoding and SPAKE2 pairing primitives. The source is dual licensed, and this integration selects Apache-2.0. The small `LocalAdbPairing` adapter owns its loopback socket and provides cancellation, socket deadlines and bounded packet decoding while reusing the library's cryptographic implementation.

- LibADB Android 3.1.1 source and license are at https://github.com/MuntashirAkon/libadb-android/tree/3.1.1.
- Its SPAKE2 Android 2.2.1 dependency source and LGPL-3.0 license are at https://github.com/MuntashirAkon/spake2-java/tree/2.2.1.
- Conscrypt Android 2.5.3 provides the bundled TLS provider and TLS exported key material. Its Apache-2.0 source is at https://github.com/google/conscrypt/tree/2.5.3.
- Bouncy Castle 1.81 supplies the transitive HKDF/AES implementation. Its MIT license and source are at https://github.com/bcgit/bc-java/tree/r1rv81.

No host address is accepted from a remote command. Discovery selects this phone's advertised services and pairing/activation always connects to `127.0.0.1`. The RSA private key stays in AndroidKeyStore. Pairing codes use the local Android notification response and are never stored in configuration or terminal receipts.
