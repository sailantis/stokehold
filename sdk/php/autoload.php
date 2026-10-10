<?php

declare(strict_types=1);

/**
 * A dependency-free PSR-4 autoloader for the Stokehold\ namespace.
 *
 * The SDK is deliberately stdlib-only: no extensions, no FFI, no build step, and
 * no Composer needed just to *run* a worker. An application that manages its
 * dependencies with Composer should require this package instead and let its own
 * autoloader do the work; this file exists so a worker can be started by a bare
 * `php worker.php` on a machine where Composer has never been installed.
 *
 * `composer.json` maps the same namespace to `src/`, so both paths resolve to
 * the same classes.
 */

spl_autoload_register(static function (string $class): void {
    $prefix = 'Stokehold\\';

    if (!str_starts_with($class, $prefix)) {
        return;
    }

    $relative = substr($class, strlen($prefix));
    $path = __DIR__ . '/src/' . str_replace('\\', '/', $relative) . '.php';

    if (is_file($path)) {
        require $path;
    }
});
