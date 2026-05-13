# TODO

## SabreTools.Serialization — additional disc image format support

Added in v0.2.3 (cross-referenced against SabreTools / Aaru / Wiimm):
- WBFS (Wii Backup File System) — full read + filesystem browse
- BlindWrite 5/6 (.b5t/.b6t) — parse + browse via companion .b5i/.b6i
- UIF (MagicISO compressed) — full read + browse
- CIF (Easy CD Creator) — parse + browse (data embedded in .cif)
- AaruFormat (.aif) — detect-only (complex multi-codec format)
- Redumper raw dumps (.sdram/.sbram) — detect-only (requires EFM/EDC decode)
- WUX (Wii U compressed) + WUD (uncompressed) — SI partition browse/extract; title key auto-loaded from .key file (AES-128-CBC IV=0); GM partition pending
- Skeleton / Skeleton.zst — full read + browse
