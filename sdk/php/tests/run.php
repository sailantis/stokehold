<?php

declare(strict_types=1);

/**
 * Run the SDK test suite.
 *
 *     php tests/run.php
 *
 * Each test file reports its own assertions and exits non-zero on failure, so
 * this runs them as separate processes: a fatal error in one cannot prevent the
 * others from reporting, and a test that deliberately provokes a protocol error
 * cannot leave the harness in a strange state.
 *
 * No PHPUnit, by design — the SDK is stdlib-only, and a suite that needed a
 * dependency would undercut that. See the README.
 */

$tests = glob(__DIR__ . '/*Test.php');
if ($tests === false || $tests === []) {
    fwrite(STDERR, "no tests found\n");
    exit(1);
}
sort($tests);

$failed = 0;
foreach ($tests as $test) {
    $command = escapeshellarg(PHP_BINARY)
        . ' -d error_reporting=E_ALL -d display_errors=1 ' . escapeshellarg($test);
    $output = [];
    $status = 0;
    exec($command . ' 2>&1', $output, $status);

    // Warnings are expected from the tests that exercise display_errors, so the
    // suite prints the whole output only when something actually failed.
    if ($status !== 0) {
        $failed++;
        fwrite(STDOUT, implode("\n", $output) . "\n");
    } else {
        foreach ($output as $line) {
            if (str_starts_with($line, 'ok ')) {
                fwrite(STDOUT, $line . "\n");
            }
        }
    }
}

if ($failed !== 0) {
    fwrite(STDOUT, "\n{$failed} test file(s) failed\n");
    exit(1);
}

fwrite(STDOUT, "\nall test files passed\n");
exit(0);
