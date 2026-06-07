docker build -t chatdex-mcp .
docker run -d --rm -p 3000:3000 `
  -e CHATDEX_CONFIG=/app/config.toml `
  -v "${PWD}/config.toml:/app/config.toml:ro" `
  -v "${PWD}/test_fixtures/game:/workspace/game:ro" `
  -v "${PWD}/test_fixtures/blog:/workspace/blog:ro" `
  --read-only --tmpfs /tmp --cap-drop ALL `
  --security-opt no-new-privileges:true `
  chatdex-mcp
Or with docker-compose up -d.
Verify it works: curl http://localhost:3000/health → OK.
2. Expose to ChatGPT via Cloudflare Tunnel
Install cloudflared and run:
cloudflared tunnel --url http://localhost:3000
It prints a URL like https://random-name.trycloudflare.com. Use that as the MCP server URL in ChatGPT.
3. Add to ChatGPT
In ChatGPT's MCP configuration, add the MCP server with:
https://random-name.trycloudflare.com/mcp
ChatGPT will call initialize → tools/list → tools/call automatically.
