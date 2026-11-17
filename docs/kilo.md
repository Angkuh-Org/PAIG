# Kilo free and keyed models

`kilo-free/<id>` lists the keyless catalogue models: only ids ending in `:free`
are accepted. `kilo/<id>` and `kilocode/<id>` are keyed and require
`KILO_API_KEY`; a keyed-only id can still be requested directly through them.

The catalogue is fetched without credentials. With a key, `KILO_ALL_MODELS=1`
exposes the full paid catalogue under `kilo/`; free models always live under
`kilo-free/`.
