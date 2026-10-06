# wifi

The wifi manager: known networks in `/etc/wifi/<network>`, the kernel's
`/dev/wifi0` control device, and `wifi auto` as a herd service. There is no
supplicant: the kernel does the WPA2 handshake.

The main doc is [`userspace/wifi/README.md`](../../userspace/wifi/README.md)
(commands, the service, building and testing).

See also: [`../reference/subsystems/wifi.md`](../reference/subsystems/wifi.md)
(the device protocol, the simulated radio, the config file format) and
[`proposals/AKUMA_WIFI_CONTROL.md`](../../proposals/AKUMA_WIFI_CONTROL.md)
(the design).
