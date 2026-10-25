# `disc` API (47)

Optional discovery/identity policy for standalone devices and experiments.
The companion normally supplies signed records; this module retains the former
NAN local-announcement builder as an optional supplier. Crypto and private-key
storage remain native runtime services. It owns no radio or radio cadence, peer
inventory or NAN internals. Native PPP port 245.

Start parameters: optional CBOR `{1: channel_hint}`; default 6, range 1..13.
Start generates one cached signed local record. It does not start NAN or publish
automatically. Stop releases the cache and any bounded publication transaction.
Name/domain and actual Wi-Fi facts are sampled only on creation/explicit refresh.
The current record/identity format remains compatible with Android/shared codec;
certificate-chain policy and other discovery experiments are future extensions.

## 1. `announce` — read cached local record

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `refresh` | `bool` | Default false; regenerate/sign before returning. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `record` | `bytes` | Cached signed discovery envelope. |

Refresh failure retains the old cache. Ordinary reads never sign. Changing this
cache alone does not replace an installed NAN publication.

## 2. `publish` — install cached record through NAN's public method

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `refresh` | `bool` | Default false; regenerate/sign first. |

Returns an empty map only after NAN acknowledges `set_announce` (46.11).
NAN must already be started. One transaction at a time; a second returns BUSY.
The two-second response deadline bounds transaction storage. Timeout returns
BUSY and leaves application completion unknown; query `nan.announce` to verify
whether the record was installed. A queued message alone is not success.
Stop cancels the wait, not a request already accepted by NAN. NAN retains its
own cached bytes independently if this supplier stops or is updated.

Publication requires the asynchronous native PPP/module-message path. A
synchronous application `xip_instance_request` (including FW's synchronous
module bridge) is rejected before queuing: it cannot wait for another module
from the shared worker. Such a caller can fetch `disc.announce` and invoke
`nan.set_announce` as two ordinary immediate requests instead. No runtime
request/worker ABI change is needed.

Publication requires the asynchronous native PPP/module-message path. A
synchronous application `xip_instance_request` (including FW's synchronous
module bridge) is rejected before queuing: it cannot wait for another module
from the shared worker. Such a caller can fetch `disc.announce` and invoke
`nan.set_announce` as two ordinary immediate requests instead. No runtime
request/worker ABI change is needed.
