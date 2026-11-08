# mod_wasm3

Runs WebAssembly programs with the wasm3 interpreter. Method 1 (`run`) takes
inline bytes. Method 2 (`run_file`) loads a named program through the runtime
files service, supplied by mod_flash. Files may exceed one packet.

```sh
dmesh-cli "$PORT" wasm3.run_file --name=wasm
```

This loads the file named `wasm`. Both methods accept an export name, integer
arguments and a memory cap; see [API.md](API.md). Replies contain the result,
output and status. Missing files or storage failures return a negative ABI error.
