"""What the agent CLI starts as its MCP server: pipes stdin and stdout to
the harness's proxy on 127.0.0.1:<port> (see proxy.py).

    python3 -I relay.py <port>
"""

import os
import socket
import sys
import threading


def main():
    connection = socket.create_connection(("127.0.0.1", int(sys.argv[1])))

    def upstream():
        for line in sys.stdin.buffer:
            connection.sendall(line)
        # The CLI closed our input: it is done with the server.
        connection.close()
        os._exit(0)

    threading.Thread(target=upstream, daemon=True).start()
    with connection.makefile("rb") as stream:
        for line in stream:
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()


if __name__ == "__main__":
    try:
        main()
    except (KeyboardInterrupt, BrokenPipeError, ConnectionError):
        pass
