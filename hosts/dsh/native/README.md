# Where `libbas_api` lives

`lib/native.js` loads the protocol layer from this directory (after
`$BAS_NATIVE_LIB`, which always wins). Drop the built library here:

```
native/libbas_api.so      # Linux
native/libbas_api.dylib   # macOS
```

Build it with `npm run build:native` (that is `cargo build --release -p bas-api`)
and copy the artifact from `target/release/`.

The plugin also searches `lib/` and the package root, so a development checkout
can point `BAS_NATIVE_LIB` at `target/release/libbas_api.so` and skip the copy.

Nothing here is required at install time: a plugin whose library is missing
still loads and registers its tools, and each call fails with
`unavailable — <how to build it>` instead of crashing.
