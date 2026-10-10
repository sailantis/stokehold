<?php

declare(strict_types=1);

/**
 * A minimal stokehold worker.
 *
 * Run it directly — there is nothing to install:
 *
 *     php example/worker.php
 *
 * It will print a handshake banner to stderr and then wait, doing nothing,
 * because a worker speaks the protocol over stdin/stdout rather than HTTP. To
 * drive it you need the server (`stokehold serve`) or the protocol test in
 * crates/stokehold-worker/tests/php_worker.rs.
 *
 * Nothing in here is stokehold-specific except `run()`: this is an ordinary PHP
 * file that happens to stay resident. Note that it `echo`s and returns `null` in
 * places, which works because the SDK captures output and uses it as the body —
 * an application does not have to be rewritten to become worker-safe.
 */

require __DIR__ . '/../autoload.php';

use Stokehold\Request;
use Stokehold\Response;
use Stokehold\Worker;

$worker = Worker::create();

$worker->run(static function (Request $request, Worker $worker): Response|null {
    $path = (string) parse_url($request->uri, PHP_URL_PATH);

    // Health: return a Response explicitly.
    if ($path === '/health') {
        return Response::json(['status' => 'ok', 'pid' => getmypid()]);
    }

    // Telemetry travels on its own channel, so it never touches the response.
    $worker->emitMetric('example.request', 1.0, 'count');
    $worker->emitLog("serving {$request->method} {$request->uri}");

    // Echo case: print something and return null. The captured output becomes
    // the response body, so a plain PHP script needs no changes at all.
    //
    // The raw body is echoed back too — not something a real application should
    // do with untrusted input, but this route exists to be probed, and seeing the
    // bytes is the only way to confirm a body arrived intact rather than merely
    // arriving.
    if ($path === '/echo') {
        echo 'you asked for ', htmlspecialchars($request->uri, ENT_QUOTES), "\n";
        echo 'method: ', $request->method, "\n";
        echo 'body: ', strlen($request->body), " bytes\n";
        echo 'payload: ', $request->body, "\n";

        return null;
    }

    // Ask the server to replace this worker once the request is done. The SDK
    // reports `keep_alive = false` in END and the pool boots a fresh process,
    // which is what an application does when it has put itself into a state it
    // cannot clean up: a leaked global, a stuck connection, a fat static cache.
    if ($path === '/recycle') {
        $worker->recycle();

        return Response::json(['recycled' => true, 'pid' => getmypid()]);
    }

    // A deliberately broken route, to show that one failing request does not
    // take the worker with it: the SDK turns it into a 500 and keeps serving.
    if ($path === '/boom') {
        throw new RuntimeException('this route always fails');
    }

    // A body larger than one protocol chunk, so a reader can confirm the whole
    // thing arrives: chunking is where a length/flag disagreement would show up.
    if ($path === '/big') {
        return Response::text(str_repeat('abcdefgh', 100000));
    }

    return Response::html(
        '<h1>stokehold worker</h1>'
        . '<p>Method ' . htmlspecialchars($request->method, ENT_QUOTES) . '</p>'
        . '<p>URI ' . htmlspecialchars($request->uri, ENT_QUOTES) . '</p>'
        . '<p>PHP ' . PHP_VERSION . ' on ' . PHP_SAPI . '</p>'
    );
});
