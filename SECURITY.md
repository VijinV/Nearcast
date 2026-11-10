# Security Policy

## Supported versions

Only the latest release gets security fixes.

## Reporting a vulnerability

**Please don't open a public issue for security problems.**

Report them privately through GitHub: go to the repository's **Security** tab and click **Report a vulnerability**. If that isn't available, contact a maintainer through their GitHub profile.

Please include:
- what an attacker can do, and what they need to have or know (for example, being on the same network)
- steps to reproduce
- the version and OS you tested

You'll get an acknowledgement within a few days. Fixes are released as soon as practical, and reporters are credited unless they'd rather not be.

## Threat model: please read

Nearcast is built for **trusted local networks** such as home or office Wi-Fi.

- **Anyone on the same network who can reach the app's port can watch while sharing is on.** There's no login or PIN yet.
- Traffic to viewers on the LAN uses plain **HTTP and WebSocket**, so the "sharp text" mode isn't encrypted. Video-mode media uses WebRTC, which is always encrypted (DTLS-SRTP), but signaling goes over plain HTTP.
- Don't share on public or untrusted Wi-Fi, and click **Stop sharing** when you're done.

Reports that bypass those expectations are still welcome. For example: viewers reaching the stream from outside the LAN, a viewer seeing anything other than the shared screen, or code execution through the viewer page or the signaling endpoint.
