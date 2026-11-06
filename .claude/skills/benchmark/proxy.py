"""TCP proxy adding a fixed one-way delay per direction"""
import asyncio, sys, time
LISTEN, TARGET, DELAY = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3]) / 1000

async def pipe(reader, writer):
    queue = asyncio.Queue()
    async def send():
        while True:
            due, data = await queue.get()
            if data is None:
                writer.close(); return
            wait = due - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            writer.write(data)
            await writer.drain()
    task = asyncio.create_task(send())
    while True:
        data = await reader.read(65536)
        queue.put_nowait((time.monotonic() + DELAY, data or None))
        if not data:
            break
    await task

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection("127.0.0.1", TARGET)
    await asyncio.gather(pipe(cr, sw), pipe(sr, cw), return_exceptions=True)

async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", LISTEN)
    async with server:
        await server.serve_forever()
asyncio.run(main())
