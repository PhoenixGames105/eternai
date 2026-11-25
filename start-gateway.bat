@echo off
rem Starts the eternAI gateway with the live configuration:
rem   - working directory C:\Users\phoenix (store, journal, memory live there)
rem   - TLS on port 8443, SearXNG at 127.0.0.1:5678
rem   - pairing automatically advertises the Tailscale IP when Tailscale is connected
cd /d C:\Users\phoenix
start "" "E:\eternAI\target\debug\etern-gateway.exe" --tls-listen=8443 --searxng=http://127.0.0.1:5678
