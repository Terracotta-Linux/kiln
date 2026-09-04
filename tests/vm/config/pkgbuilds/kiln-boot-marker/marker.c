/* The payload of the boot acceptance test's built package.
 *
 * It is C rather than a shell script on purpose: compiling it is the only way
 * to find out whether the build root realization assembles actually has a
 * working toolchain in it. A `package()` that only ran `install` would prove the
 * plumbing and nothing about the room it runs in. */
#include <stdio.h>

int main(void) {
    puts("built by kiln from a PKGBUILD");
    return 0;
}
