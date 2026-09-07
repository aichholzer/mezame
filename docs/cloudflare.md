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

5. Tell Mezame the hostname. `cloudflared` passes it through as the `Host` of every request, and Mezame answers 421 to a hostname it has not been told about (that check is what stops DNS rebinding). Add it to the transport entry in `~/.mezame/config.json` and restart Mezame:

   ```json
   {
     "transports": [
       { "kind": "cloudflared", "bind": "127.0.0.1:9510", "hosts": ["mezame.example.com"] }
     ]
   }
   ```

6. Run it:

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

Reload `cloudflared`. WebSocket upgrades are forwarded by default and `/ws` needs no special flags.

This recipe puts `cloudflared` on one machine and Mezame on another, so Mezame has to bind an address that machine can reach (`0.0.0.0:9510` or a LAN address), and Cloudflare Access gates only the public hostname. Every host on that network segment reaches port 9510 directly with no Access in the way, and Mezame has no auth of its own: such a peer can list your sessions from `GET /state`, read any transcript, attach to any session and rewrite the shared state. When the network is not one you trust end to end, run `cloudflared` on the Mezame host with `service: http://localhost:9510` and a loopback bind, or firewall port 9510 to the `cloudflared` host.

Then list the hostname under `hosts` in the transport entry of Mezame's `~/.mezame/config.json` (step 5 above shows the shape) and restart Mezame. Without it, every request arriving through the tunnel is answered 421, because Mezame serves only hostnames it has been told about. If your ingress rule sets `originRequest.httpHostHeader` instead, the entry is still needed: the browser's `Origin` carries the public hostname, and Mezame accepts an upgrade or a write from a listed hostname whatever `Host` was rewritten to.

## Put Cloudflare Access in front (strongly recommended)

Once a public hostname points at Mezame, anyone who finds the URL can drive your local agent. Treat this as non-optional:

1. Cloudflare Zero Trust, Access, Applications, Add application, Self-hosted.
2. Application domain: `mezame.example.com`.
3. Policy: allow only your email, passkey, or IdP identity.

Access injects a signed `Cf-Access-Jwt-Assertion` header on every request. Mezame does not validate the session today; see the "Auth enforcement" entry under Known gaps in the main README.
