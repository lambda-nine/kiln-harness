#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "config.h"

static int port = DEFAULT_PORT;
static const char *root = DEFAULT_ROOT;

static void usage(const char *name)
{
	fprintf(stderr, "usage: %s [-p port] [-d dir]\n", name);
	exit(2);
}

static int parse_port(const char *s)
{
	int p = atoi(s);
	if (p <= 0 || p > 65535)
		return -1;
	return p;
}

int main(int argc, char **argv)
{
	int i;

	for (i = 1; i < argc; i++) {
		if (strcmp(argv[i], "-p") == 0 && i + 1 < argc) {
			port = parse_port(argv[++i]);
			if (port < 0)
				usage(argv[0]);
		} else if (strcmp(argv[i], "-d") == 0 && i + 1 < argc) {
			root = argv[++i];
		} else {
			usage(argv[0]);
		}
	}
	printf("serving %s on port %d\n", root, port);
	return 0;
}
