# ChatdexMCP

give chatbots context about your project.

![ChatdexMCP Screenshot](Screenshot%202026-06-07%20164844.png)

## dependenices
- cloudflared
- docker
## getting started
start mcp server and tunnel
```bash
cloudflared tunnel --url http://localhost:6967
docker-compose up --build
```
give public url to your choosen chatbot.
