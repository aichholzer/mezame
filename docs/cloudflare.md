# Cloudflare Tunnel and Access

Mezame has no auth of its own. The intended production posture is a named Cloudflare Tunnel fronting Mezame on loopback, with Cloudflare Access gating the public hostname. This document walks through both.

## Expose via Cloudflare Tunnel

A named Cloudflare Tunnel can route a public hostname at your local Mezame. The setup differs slightly depending on whether you already run `cloudflared`.

### Starting from scratch

1. Install `cloudflared` and authenticate:

   ```sh
   cloudflared login
   ```

2. Create a tunnel. The name is yours to pick; Cloudflare returns a UUID and writes credentials to `~/.cloudflared/<UUID>.json`:

   ```sh
   cloudflared tunnel create mezame
   ```

3. Create `~/.cloudflared/config.yml` with the following contents, replacing `REPLACE_WITH_TUNNEL_UUID` with the UUID from step 2 and `mezame.example.com` with your hostname. WebSocket upgrades are forwarded by default; no extra flags needed.

   ```yaml
   tunnel: REPLACE_WITH_TUNNEL_UUID
   credentials-file: ~/.cloudflared/REPLACE_WITH_TUNNEL_UUID.json

   ingress:
     - hostname: mezame.example.com
       service: http://localhost:9510
     - service: http_status:404
   ```

   The `tunnel:` value must match the tunnel you created; if it does not, `cloudflared` refuses to start.

4. Route the hostname to the tunnel from the machine that owns the credentials:

   ```sh
   cloudflared tunnel route dns mezame mezame.example.com
   ```

5. Run it:

   ```sh
   cloudflared tunnel run mezame
   ```

   or install it as a system service with `cloudflared service install`.

### Adding Mezame to an existing tunnel

If you already have `cloudflared` running (Proxmox LXC, Docker, systemd unit, whatever...), keep your current config and add one ingress rule above the catch-all:

```yaml
ingress:
  # ... your existing rules above ...
  - hostname: mezame.example.com
    service: http://<host-running-mezame>:9510
  # keep the catch-all last
  - service: http_status:404
```

Route the hostname once:

```sh
cloudflared tunnel route dns <your-tunnel-name> mezame.example.com
```

Reload `cloudflared`. WebSocket upgrades are forwarded by default, so `/ws` needs no special flags.

## Put Cloudflare Access in front (strongly recommended)

Once a public hostname points at Mezame, anyone who finds the URL can drive your local agent. Treat this as non-optional:

1. Cloudflare Zero Trust, Access, Applications, Add application, Self-hosted.
2. Application domain: `mezame.example.com`.
3. Policy: allow only your email, passkey, or IdP identity.

Access injects a signed `Cf-Access-Jwt-Assertion` header on every request. Mezame does not validate the session today; see the "Auth enforcement" entry under Known gaps in the main README.
