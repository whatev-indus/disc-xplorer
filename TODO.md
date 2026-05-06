# TODO

## Add optical drive emulation from CDemu

CDemu provides virtual optical drive emulation on Linux. Investigate extending this to
act as a full emulated drive (not just a mount point) so the guest OS sees a virtual
optical drive with proper drive behavior, subchannel data, etc.

## Add .sub and .mds (DPM) compatibility with bin/cue via drive emulation

`.sub` files carry subchannel data alongside `.cue`/`.bin` images. `.mds` files include
DPM (Data Position Measurement) data required by copy protection schemes (e.g. ProtectCD
v5+). Currently these are not replayed during emulation. Via drive emulation, the
subchannel and DPM streams should be fed through so that protected disc images behave
correctly when authenticated by the guest software.

## In settings add "Detach Sector View window." option

## SabreTools.Serialization — additional disc image format support

Investigate https://github.com/SabreTools/SabreTools.Serialization for formats not yet
supported by the native Rust parsers, particularly:
- Xbox / Xbox 360 disc images (XGD)
- Nintendo optical discs (NOD / GCM / WBFS)
- Any other formats with broad community support

SabreTools is C#, so direct use requires a Rust port or a subprocess call. Evaluate whether
format specs are documented well enough to write pure Rust parsers instead.
