# Source notices

The vendored source base is VirtualDrivers/Virtual-Display-Driver commit
`d437ebc9b44a14ce6e5cc9c8b7f6beb08d6faf77`. Its MIT licence, Copyright 2024
Virtual Display, is preserved verbatim in LICENSE. Microsoft Corporation notices
present in that source are retained in the adapted source files. UPSTREAM.json
records the original paths, hashes and modification mapping. Crosspane changes
use this repository's GPL-3.0-or-later licence.

The fork removes XML configuration, the named-pipe control server, file logging,
automatic monitor creation and optional HDR paths. New control, validation and
lease code implements the frozen Crosspane device-interface contract. Each
monitor owns its descriptor and swapchain processor.

Microsoft's IndirectDisplay sample at commit
`d5569c08aa2818c6240744bb47a00f67f20fdb54` is a reference for the documented
AssignSwapChain/UnassignSwapChain lifecycle. Its root licence is MS-PL. No sample
body is copied into this fork; the adapters are written from the public API
contracts. This reference does not change the vendored source licence.
