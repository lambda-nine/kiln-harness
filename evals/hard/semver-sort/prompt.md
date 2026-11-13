Write a POSIX shell script `/workspace/semver-sort.sh` that sorts version strings by Semantic Versioning 2.0.0 precedence. It will be run as `sh /workspace/semver-sort.sh < FILE`, from any directory, and reads one version per line on standard input. Use only POSIX `sh` and the standard tools available here (`awk`, `sed`, `sort`, `grep` and so on); there is no network.

## Valid versions

A line is valid only if it is a whole SemVer 2.0.0 version, with nothing before or after it (no leading `v`, no spaces or tabs). Every line, including an empty one, must be valid.

- `MAJOR.MINOR.PATCH`: three non-negative decimal integers without leading zeros (`0` itself is fine).
- Optionally a pre-release: `-` followed by one or more identifiers separated by `.`. Each identifier is non-empty and uses only `0-9`, `A-Z`, `a-z` and `-`. An identifier made only of digits is numeric and must not have leading zeros; any other identifier is alphanumeric.
- Optionally build metadata, after the version and any pre-release: `+` followed by one or more non-empty identifiers of the same characters, separated by `.`. Leading zeros are allowed here.

Numbers (major, minor, patch and numeric pre-release identifiers) can be up to 30 digits long. Compare them by numeric value, exactly.

## Order

Sort ascending by precedence, as SemVer 2.0.0 section 11 defines it:

1. Compare major, minor and patch numerically, in that order.
2. A version with a pre-release has lower precedence than the same version without one.
3. Compare two pre-releases identifier by identifier from left to right: numeric identifiers by numeric value, alphanumeric identifiers by ASCII byte order, and a numeric identifier is lower than an alphanumeric one. If all identifiers so far are equal, the pre-release with fewer identifiers is lower.
4. Build metadata is ignored. Versions of equal precedence keep their input order.

For example, `1.0.0-alpha < 1.0.0-alpha.1 < 1.0.0-alpha.beta < 1.0.0-beta < 1.0.0-beta.2 < 1.0.0-beta.11 < 1.0.0-rc.1 < 1.0.0`.

## Output and errors

- Print every input line exactly as it was, in sorted order, each followed by a single LF. Input lines end in LF, but the last line may have no LF.
- Empty input: print nothing and exit 0.
- If any line is invalid: print nothing on standard output, print exactly one line `invalid version on line N: TEXT` on standard error for the first invalid line (N counts from 1, TEXT is the line as given), and exit 2.
- Otherwise exit 0.

`/workspace/examples.txt` is a sample input, and `/workspace/examples.sorted.txt` is its correct output. Commands you run start in `/workspace`.
