package com.infrawrench.opencomputeruse.host;

import android.content.Context;
import android.util.Log;

import org.json.JSONArray;
import org.json.JSONObject;
import org.json.JSONTokener;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;

/**
 * The phone's HTTP server, the same API as the desktop's (src/remote/
 * server.rs), so a device set up to drive computers drives the phone too:
 *
 * - `POST /v1/tools/<tool>` with the arguments as JSON, answered with the
 *   `{content, isError}` an MCP tool call gives; `GET /v1/tools` lists them.
 * - `POST /mcp`: MCP's streamable HTTP transport, answered with plain JSON.
 * - `GET /health`, the one route that needs no key.
 *
 * Each device's requests run in order on a worker of its own, which owns
 * the device's sessions; regenerating or removing its key ends them.
 */
final class HttpServer {
    private static final String TAG = "OcuHttp";
    private static final int MAX_BODY = 1 << 20;
    private static final int MAX_HEADERS = 64 * 1024;

    private static HttpServer running;

    private final Context ctx;
    private final int port;
    private final ServerSocket socket;
    private final Thread acceptor;
    private volatile boolean stopped;

    /** A device's worker: one thread, and the sessions it owns. */
    private static final class Worker {
        final ExecutorService thread;
        final Tools.Client client = new Tools.Client();

        Worker(String name) {
            thread = Executors.newSingleThreadExecutor(r -> {
                Thread t = new Thread(r, "ocu-device-" + name);
                t.setDaemon(true);
                return t;
            });
        }

        void stop() {
            thread.execute(client::endAll);
            thread.shutdown();
        }
    }

    private List<Keys.Device> devices = new ArrayList<>();
    private long devicesModified = -1;
    /** Keyed by device id and key hash, so a new key gets a fresh worker. */
    private final Map<String, Worker> workers = new HashMap<>();

    private HttpServer(Context ctx, int port) throws IOException {
        this.ctx = ctx.getApplicationContext();
        this.port = port;
        socket = new ServerSocket();
        socket.setReuseAddress(true);
        socket.bind(new InetSocketAddress(port));
        acceptor = new Thread(this::accept, "ocu-http");
        acceptor.setDaemon(true);
        acceptor.start();
        Log.i(TAG, "listening on port " + port);
    }

    /** Starts the server on `port`, or moves it there; a no-op when it is. */
    static synchronized void start(Context ctx, int port) throws IOException {
        if (running != null && running.port == port && !running.stopped) {
            return;
        }
        stop();
        running = new HttpServer(ctx, port);
    }

    static synchronized void stop() {
        if (running != null) {
            running.close();
            running = null;
        }
    }

    static synchronized boolean isRunning() {
        return running != null && !running.stopped;
    }

    static synchronized int runningPort() {
        return running == null ? 0 : running.port;
    }

    private void close() {
        stopped = true;
        try {
            socket.close();
        } catch (IOException ignored) {
        }
        synchronized (workers) {
            for (Worker w : workers.values()) {
                w.stop();
            }
            workers.clear();
        }
        Log.i(TAG, "stopped on port " + port);
    }

    private void accept() {
        while (!stopped) {
            try {
                Socket s = socket.accept();
                Thread t = new Thread(() -> serve(s), "ocu-http-request");
                t.setDaemon(true);
                t.start();
            } catch (IOException e) {
                if (!stopped) {
                    Log.w(TAG, "accept: " + e);
                }
            }
        }
    }

    // ------------------------------------------------------------ devices

    /** The device a key belongs to, rereading devices.json when it changed
     *  and retiring workers whose device or key is gone. */
    private Keys.Device authenticate(String key) {
        synchronized (workers) {
            File f = Keys.file(ctx);
            long mtime = f.isFile() ? f.lastModified() : 0;
            if (mtime != devicesModified || mtime == 0) {
                devices = Keys.load(ctx);
                devicesModified = mtime;
                List<String> valid = new ArrayList<>();
                for (Keys.Device d : devices) {
                    valid.add(d.id + "/" + d.keyHash);
                }
                List<String> gone = new ArrayList<>();
                for (String k : workers.keySet()) {
                    if (!valid.contains(k)) {
                        gone.add(k);
                    }
                }
                for (String k : gone) {
                    workers.remove(k).stop();
                }
            }
            return key == null ? null : Keys.authenticate(devices, key);
        }
    }

    private <T> T onWorker(Keys.Device d, Callable<T> job) throws Exception {
        Future<T> f;
        synchronized (workers) {
            String k = d.id + "/" + d.keyHash;
            Worker w = workers.get(k);
            if (w == null) {
                w = new Worker(d.name);
                workers.put(k, w);
            }
            f = w.thread.submit(job);
        }
        return f.get();
    }

    private Tools.Client clientOf(Keys.Device d) {
        synchronized (workers) {
            Worker w = workers.get(d.id + "/" + d.keyHash);
            return w == null ? new Tools.Client() : w.client;
        }
    }

    // ---------------------------------------------------------------- HTTP

    private static final class Request {
        String method;
        String path;
        final Map<String, String> headers = new HashMap<>();
        byte[] body = new byte[0];
        boolean tooLarge;
    }

    private static String readLine(InputStream in, int[] budget) throws IOException {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        int c;
        while ((c = in.read()) != -1) {
            if (--budget[0] < 0) {
                throw new IOException("headers too large");
            }
            if (c == '\n') {
                break;
            }
            if (c != '\r') {
                b.write(c);
            }
        }
        if (c == -1 && b.size() == 0) {
            return null;
        }
        return new String(b.toByteArray(), StandardCharsets.ISO_8859_1);
    }

    private static Request read(InputStream in) throws IOException {
        int[] budget = {MAX_HEADERS};
        String line = readLine(in, budget);
        if (line == null || line.isEmpty()) {
            return null;
        }
        String[] parts = line.split(" ");
        if (parts.length < 2) {
            return null;
        }
        Request r = new Request();
        r.method = parts[0].toUpperCase(Locale.ROOT);
        String target = parts[1];
        int q = target.indexOf('?');
        r.path = q < 0 ? target : target.substring(0, q);
        String h;
        while ((h = readLine(in, budget)) != null && !h.isEmpty()) {
            int colon = h.indexOf(':');
            if (colon > 0) {
                r.headers.put(h.substring(0, colon).trim().toLowerCase(Locale.ROOT),
                        h.substring(colon + 1).trim());
            }
        }
        ByteArrayOutputStream body = new ByteArrayOutputStream();
        String te = r.headers.get("transfer-encoding");
        if (te != null && te.toLowerCase(Locale.ROOT).contains("chunked")) {
            while (true) {
                String size = readLine(in, new int[] {1024});
                if (size == null) {
                    break;
                }
                int semi = size.indexOf(';');
                int n = Integer.parseInt((semi < 0 ? size : size.substring(0, semi)).trim(), 16);
                if (n == 0) {
                    while ((h = readLine(in, new int[] {MAX_HEADERS})) != null && !h.isEmpty()) {
                        // Trailers are ignored.
                    }
                    break;
                }
                if (body.size() + n > MAX_BODY) {
                    r.tooLarge = true;
                    return r;
                }
                copy(in, body, n);
                readLine(in, new int[] {16});
            }
        } else {
            String cl = r.headers.get("content-length");
            long n = 0;
            if (cl != null) {
                try {
                    n = Long.parseLong(cl.trim());
                } catch (NumberFormatException e) {
                    n = 0;
                }
            }
            if (n > MAX_BODY) {
                r.tooLarge = true;
                return r;
            }
            copy(in, body, (int) n);
        }
        r.body = body.toByteArray();
        return r;
    }

    private static void copy(InputStream in, ByteArrayOutputStream out, int n) throws IOException {
        byte[] buf = new byte[8192];
        while (n > 0) {
            int got = in.read(buf, 0, Math.min(buf.length, n));
            if (got < 0) {
                throw new IOException("the body ended early");
            }
            out.write(buf, 0, got);
            n -= got;
        }
    }

    private static String reason(int status) {
        switch (status) {
            case 200: return "OK";
            case 202: return "Accepted";
            case 400: return "Bad Request";
            case 401: return "Unauthorized";
            case 404: return "Not Found";
            case 405: return "Method Not Allowed";
            default: return "Internal Server Error";
        }
    }

    private static void respond(OutputStream out, int status, Object body, boolean challenge)
            throws IOException {
        byte[] data = body == null ? new byte[0]
                : Json.compact(body).getBytes(StandardCharsets.UTF_8);
        StringBuilder h = new StringBuilder();
        h.append("HTTP/1.1 ").append(status).append(' ').append(reason(status)).append("\r\n");
        if (body != null) {
            h.append("Content-Type: application/json\r\n");
        }
        if (challenge) {
            h.append("WWW-Authenticate: Bearer\r\n");
        }
        h.append("Content-Length: ").append(data.length).append("\r\n");
        h.append("Connection: close\r\n\r\n");
        out.write(h.toString().getBytes(StandardCharsets.ISO_8859_1));
        out.write(data);
        out.flush();
    }

    private static JSONObject error(String message) {
        JSONObject o = new JSONObject();
        try {
            o.put("error", message);
        } catch (Exception ignored) {
        }
        return o;
    }

    private static Object parseJson(byte[] body) throws Exception {
        String text = new String(body, StandardCharsets.UTF_8);
        if (text.trim().isEmpty()) {
            return new JSONObject();
        }
        try {
            Object v = new JSONTokener(text).nextValue();
            if (v instanceof JSONObject || v instanceof JSONArray) {
                return v;
            }
        } catch (Exception ignored) {
        }
        throw new Exception("the request body is not JSON");
    }

    private void serve(Socket s) {
        try (Socket sock = s) {
            sock.setSoTimeout(30_000);
            InputStream in = new BufferedInputStream(sock.getInputStream());
            OutputStream out = sock.getOutputStream();
            Request r = read(in);
            if (r == null) {
                return;
            }
            // Requests can take a while (a tool call); the client waits.
            sock.setSoTimeout(0);
            handle(r, out);
        } catch (Exception e) {
            Log.w(TAG, "request: " + e);
        }
    }

    private void handle(Request r, OutputStream out) throws IOException {
        if (r.path.equals("/health")) {
            JSONObject ok = new JSONObject();
            try {
                ok.put("ok", true);
            } catch (Exception ignored) {
            }
            respond(out, 200, ok, false);
            return;
        }
        String auth = r.headers.get("authorization");
        String key = auth != null && auth.startsWith("Bearer ") ? auth.substring(7).trim() : null;
        Keys.Device device = authenticate(key);
        if (device == null) {
            respond(out, 401, error("missing or unknown key; send Authorization: Bearer <key>"), true);
            return;
        }
        try {
            if (r.method.equals("GET") && r.path.equals("/v1/tools")) {
                JSONObject o = new JSONObject();
                o.put("tools", Tools.list());
                respond(out, 200, o, false);
            } else if (r.method.equals("POST") && r.path.startsWith("/v1/tools/")) {
                String name = r.path.substring("/v1/tools/".length());
                Object args;
                try {
                    if (r.tooLarge) {
                        throw new Exception("the request body is too large");
                    }
                    args = parseJson(r.body);
                } catch (Exception e) {
                    respond(out, 400, error(e.getMessage()), false);
                    return;
                }
                final JSONObject a = args instanceof JSONObject ? (JSONObject) args : new JSONObject();
                JSONObject result;
                try {
                    result = onWorker(device, () -> Mcp.callTool(ctx, clientOf(device), name, a));
                } catch (Exception e) {
                    respond(out, 500, error(e.toString()), false);
                    return;
                }
                respond(out, 200, result, false);
            } else if (r.method.equals("POST") && r.path.equals("/mcp")) {
                Object msg;
                try {
                    if (r.tooLarge) {
                        throw new Exception("the request body is too large");
                    }
                    msg = parseJson(r.body);
                } catch (Exception e) {
                    JSONObject err = new JSONObject();
                    err.put("jsonrpc", "2.0");
                    err.put("id", JSONObject.NULL);
                    err.put("error", new JSONObject().put("code", -32700).put("message", e.getMessage()));
                    respond(out, 400, err, false);
                    return;
                }
                Object reply;
                try {
                    reply = onWorker(device, () -> Mcp.dispatchValue(ctx, clientOf(device), msg));
                } catch (Exception e) {
                    respond(out, 500, error(e.toString()), false);
                    return;
                }
                if (reply == null) {
                    // Notifications and responses get no body.
                    respond(out, 202, null, false);
                } else {
                    respond(out, 200, reply, false);
                }
            } else if (r.method.equals("GET") && r.path.equals("/mcp")) {
                respond(out, 405, error("this server does not stream; POST JSON-RPC to /mcp"), false);
            } else {
                respond(out, 404, error("no route " + r.method + " " + r.path), false);
            }
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            respond(out, 500, error(e.toString()), false);
        }
    }
}
