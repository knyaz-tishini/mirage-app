# mirage2app

Mirage VPN client: CLI + client-core library. Extracted from mirage2web.

## Building

```bash
cd mirage2app && cargo build --release -p mirage-client-cli
```

## Running

```bash
mirage-client-cli.exe --config ../mirage2tun/client-config.json
```

## Testing chains

```bash
curl -x socks5h://127.0.0.1:19480 http://ifconfig.me   # default (byte-cipher-classic)
curl -x socks5h://127.0.0.1:19482 http://ifconfig.me   # classic-direct
curl -x socks5h://127.0.0.1:19483 http://ifconfig.me   # crypto-relay-classic
curl -x socks5h://127.0.0.1:19484 http://ifconfig.me   # three-hop-onion
curl -x socks5h://127.0.0.1:19486 http://ifconfig.me   # three-hop-telescopic
curl -x http://127.0.0.1:19481 http://ifconfig.me       # HTTP CONNECT proxy
```

All should return `45.59.119.139` (L2-CH-TEST exit IP).
