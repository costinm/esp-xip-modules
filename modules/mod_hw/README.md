# mod_hw

Hardware-policy module for the [esp-xip-modules](https://github.com/costinm/esp-xip-modules) host: battery level, ADC probing and
a GPIO button with short/long/double press events. `no_std`, only the host table (ADC, GPIO, interrupts, settings).

Requests are CBOR arrays of unsigned integers, `[1, pin, ref_mv, divider_x100, min_mv, max_mv, ctrl_pin, ctrl_level, enabled]`
for the battery (every item after the operation is optional and falls back to the module's settings:
`battery.pin`, `battery.divider`, ...), `[2, samples, interval_ms, ref_mv, pin...]` for the ADC probe and `[3, pin, enabled]`
for the button (stays running until stopped). The text `battery` or `button` works from a console. Events are CBOR
arrays (value type 5): 111 battery, 110 ADC sample, 101/102/103 short/long/double press.

```sh
EM_RUST_MODULES=1 scripts/build.sh esp32c6          # target/out/<chip>/modules/mod_hw.dmod, then sign and install
cargo test                                         # the module against a fake host table
```

Same tooling and slot variables as [mod_lora](../mod_lora/README.md). Checked on an ESP32 board (lora3): a battery
reading is emitted as event 111; the pin default is GPIO35 (ESP32) / GPIO0 (ESP32-C6).
