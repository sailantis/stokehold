<?php

declare(strict_types=1);

/**
 * A tiny assertion harness.
 *
 * No PHPUnit: the SDK is stdlib-only by design, and a test suite that needed a
 * dependency would undermine that. Run it with `php tests/run.php`.
 */

final class Assert
{
    public static int $passed = 0;

    /** @var list<string> */
    public static array $failures = [];

    public static function same(mixed $expected, mixed $actual, string $what): void
    {
        if ($expected === $actual) {
            self::$passed++;

            return;
        }

        self::$failures[] = sprintf(
            "%s\n    expected: %s\n    actual:   %s",
            $what,
            self::describe($expected),
            self::describe($actual),
        );
    }

    public static function true(bool $condition, string $what): void
    {
        self::same(true, $condition, $what);
    }

    public static function contains(string $needle, string $haystack, string $what): void
    {
        if (str_contains($haystack, $needle)) {
            self::$passed++;

            return;
        }

        self::$failures[] = sprintf(
            "%s\n    expected to contain: %s\n    in: %s",
            $what,
            $needle,
            substr($haystack, 0, 300),
        );
    }

    /**
     * Assert that a callable throws, and optionally that the message matches.
     */
    public static function throws(callable $callable, string $what, ?string $needle = null): void
    {
        try {
            $callable();
        } catch (\Throwable $thrown) {
            if ($needle !== null && !str_contains($thrown->getMessage(), $needle)) {
                self::$failures[] = sprintf(
                    "%s\n    threw, but the message did not contain %s\n    got: %s",
                    $what,
                    $needle,
                    $thrown->getMessage(),
                );

                return;
            }
            self::$passed++;

            return;
        }

        self::$failures[] = "{$what}\n    expected an exception, none was thrown";
    }

    private static function describe(mixed $value): string
    {
        if (is_bool($value)) {
            return $value ? 'true' : 'false';
        }
        if (is_string($value)) {
            return "'" . $value . "'";
        }

        return var_export($value, true);
    }

    public static function report(string $name): int
    {
        if (self::$failures === []) {
            fwrite(STDOUT, 'ok   ' . $name . ' (' . self::$passed . " assertions)\n");

            return 0;
        }

        fwrite(STDOUT, "FAIL {$name}\n");
        foreach (self::$failures as $failure) {
            fwrite(STDOUT, "  - {$failure}\n");
        }

        return 1;
    }
}
