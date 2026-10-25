# `ip` API (12)

Experimental native raw-IP virtual interface for tunnel endpoints. It creates
one point-to-point link `ip`, with a static IPv4 address/netmask and/or IPv6 /64.
Modules subscribe through `Link.open` and send complete IP packets through the
same owned-packet path as other links. This component supplies no tunnel wire,
authentication, compression, default route or IP forwarding policy.

Input uses a native custom pbuf in the packet prefix; lwIP owns the original
lease until its C free callback completes TX. Output must copy the stack's
borrowed pbuf chain once into a cached native pool lease; it drops if none is
available. No separate packet queue or payload allocation. IPv4/IPv6 fragments
are refused in this experiment; tunnel endpoints must respect the configured MTU.

Only the creating module controls this interface. It contributes network
readiness for native TCP listeners even without infrastructure Wi-Fi. Owner
stop/update and network sleep teardown close it; recreate it in active mode,
not during sleepy NAN windows. Addresses/routing and work-awake policy belong
to the endpoint module. No state or pointers into module flash remain in lwIP.

## 1. `open` — create the virtual interface

**C function:** ip/Ip.open

**Request:** [Config](#config)

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `link` | `u32` | Native link address. Subscribe to `ip` before expecting outgoing packets. |

An existing interface returns BUSY. At least one address family is required.
No interface storage is reserved before open or retained after successful close.
The native link table has a finite shared capacity; open can fail if full.

## 2. `set_up` — enable or pause

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `link` | `u32` | Address returned by open. |
| 2 | `up` | `u32` | 0 down, 1 up. |

## 3. `close` — remove the interface

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `link` | `u32` | Owned interface link address. |

Removal runs synchronously on the TCP/IP thread after earlier accepted input.
Queued native frames remain C-owned until their custom pbufs are released.

# Types

## `Config`

**C type:** IpConfig

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `ipv4` | `bytes` | Four network-order bytes; zero disables IPv4. |
| 2 | `netmask` | `bytes` | Four bytes, required with IPv4. |
| 3 | `gateway` | `bytes` | Four bytes; no default-route installation is performed. |
| 4 | `ipv6` | `bytes` | Sixteen network-order bytes; zero disables IPv6. |
| 5 | `mtu` | `u16` | 0/default 1280, range 1280..1500; outer tunnel overhead must fit separately. |
