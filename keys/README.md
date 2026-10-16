# Keys

`modules.pub` is the public key of the packages this repository publishes: 130 hex digits, an uncompressed P-256 point
(`sign.py pubkey KEY.pem`). Nothing is built with it: a board pins the key of the first package it installs (esp-wifi-loader's
trust on first use), and everything after that - `main`, modules - has to be signed with the same key. The matching private key is
`~/.ssh/esp_modules/modules-signing-key.pem` on the maintainer's machine and the GitHub secret `XIP_SIGNING_KEY`; it is never
committed (`docs/releasing.md`). A developer who builds their own product signs everything with their own key.
