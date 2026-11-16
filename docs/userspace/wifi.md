# wifi

The wifi manager: known networks in `/etc/wifi/<network>`, the kernel's
`/dev/wifi0` control device, and `wifi auto` as a herd service. There is no
supplicant: the kernel does the WPA2 handshake.

The main doc is [`userspace/wifi/README.md`](../../userspace/wifi/README.md)
(commands, the service, building and testing). Review findings (atomic saves, passphrase echo,
`forget` of a broken file): [`userspace/wifi/docs/ISSUES_2026_11_16.md`](../../userspace/wifi/docs/ISSUES_2026_11_16.md).

See also: [`../reference/subsystems/wifi.md`](../reference/subsystems/wifi.md)
(the device protocol, the simulated radio, the config file format) and
[`proposals/AKUMA_WIFI_CONTROL.md`](../../proposals/AKUMA_WIFI_CONTROL.md)
(the design).
