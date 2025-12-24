# SelfCI

SelfCI is a minimalistic local-first Unix-philosophy-abiding CI.

### Status

Just started hacking on it recently, and all details can still change.

Feel free to join [`#support:dpc.pw` Matrix channel](https://matrix.to/#/#support:dpc.pw)
and I'm happy to hear your feedback and thoughts on the matter.

### Philosophy

SelfCI is meant to be very general-purpose, but it
is meant to synergize especially well with modern tools
like Jujutsu, Nix, Radicle and anything that embraced independence
and craftsmanship.

SelfCI is a **Continuous Integration** system. It is meant to
perform the same role platforms like GitHub Actions, Circle CI, etc. typically
do: make sure you can make changes to your codebase frequently and with confidence.

SelfCI is **local-first** as it is meant to be used primarily
locally, on a developer's machine, in a same way tools
like `git` are used. Most developers are able to compile the
source code their work on, run tests, etc.
Why do they remain at the mercy of a big corporation
to be allowed to run their CI on fleet of super slower VMs?

SelfCI is implemented as a single `selfci` command,
and behaves like a typical Unix tool. It
implements carefully thought through minimal set of features
needed to compose with about anything else to allow defining
CI rules in an arbitrary ways. You just need your CI to run a single
command? Easy. You want your CI rules implemented in your
favorite programming language? Easy.


### SelfCI CI rules

The base concept in SelfCI are the "base" and the "candidate".

The "base" is a source code version that is known to work.
Think `main`/`master` branch. The candidate is the code version with
proposed changes that need to be verified. Think GitHub PR.

The "candidate" is validated against the "base". It's important
to understand that the "base" defines the rules that need to
met before the "candidate" "passes the CI".


To initialize the CI run `selfci init` in the root directory of
your project's source code. This will create a `.config/selfci/ci.yaml` file
with a template to customize. Yes, I know it's yaml, and yaml ... is meh.
But it is a single yaml file, with not much structure in it, and
allows you to quickly delegate all the actual logic to something
else.

Here is an example config implementing a simple CI system:

```
job:
  command: |
    set -eou pipefail

    case "$SELFCI_JOB_NAME" in
      main)
        selfci job start "lint"
        selfci job start "cargo"
        ;;

      cargo)
        selfci step start "cargo check"
        cargo check

        selfci step start "cargo build"
        cargo build

        selfci step start "cargo nextest run"
        cargo nextest run
        ;;

      lint)
        selfci step start "Fake step"
        sleep .1
        selfci step fail --ignore
        selfci step start "Fake step2 "
        sleep .2
        selfci step fail
        selfci step start "Fake step 3"
        sleep .3
        ;;

      *)
        echo "Unknown job: $SELFCI_JOB_NAME"
        exit 1
    esac
```

I hope this bash script will not scare you.
It is meant to be just a convenient demonstration.

All CI rules are implemented through the execution
of a single script/command - so called "candidate check".

When the CI run starts this command will be executed with the `SELFCI_JOB_NAME`
environment variable set to `main`, `SELFCI_CANDIDATE_DIR` will point at
and the current working will be set to a directory which contains the copy of the candidate,
and `SELFCI_BASE_DIR` will point at a temporary copy of the base.

In SelfCI the parallel units of execution
are called "jobs" and can be started dynamically
from within existing jobs using `selfci job start <name>` at
any time. Each job will run the same command, but with a different
`SELFCI_JOB_NAME`.

This way you are in a full control of what jobs do or do
not run, and you don't need to define these upfront
using clumsy YAML DSLs.

The `selfci` command is used as the API to control the execution
of the CI rules. You don't need to call it from a bash
script. If your CI rules are complex enough, consider
building and executing a binary that calls `selfci`. The built-in
parallel execution of jobs should make implementing most
CI rules very manageable as a simple bash script.

`selfci step start <name>` informs the runner that
a new step of a job begins, allowing it to track
execution times and outcomes of individual parts
of each job. Each step is considered successful,
unless `selfci step fail [--ignore]` is called.
`--ignore` flag makes the job as a whole not
be considered as failed.

A CI run has passed if all the jobs it started
were successful. Each job is successful if
it returned a zero exit code, and no step
was marked as failed without the `--ignore` flag.

### `selfci check` & `selfci mq`

You can easily start a CI run with an
arbitrary base and candidate using `selfci check`.
This can be considered an easy way to run
the CI locally to verify any change.

Maintainers can use a `selfci mq` commands which
act more like GitHub Action's Merge Queue system.
Project maintainer can start a local CI daemon,
and then add candidates to the Merge Queue, which
the daemon will check against a base branch and
merge into the base branch if the CI passed. This
allows conveniently merging changes that were
already reviewed without the need to babysit them.


### What `selfci` does not provide

`selfci` leaves it to the user to take care of job isolation
in both senses: protecting host system from malicious code and protecting
CI jobs running in parallel from interfering.

GitHub Actions executes every job as a separate virtual machine
that makes a full copy of the source code. This is a most
conservative choice, necessary for public consumption.
The downside of it is that it is extremely wasteful.

Baking in virtual machines or even containers does not
make sense for a local-first CI. It would make everything
slow and constrained.

I don't know about you, but I typically do not think
about merging code that I did not review, and typically
I collaborate with people that I know and trust.

As you implement your CI you need to consider your specific
needs. Maybe you're going to run bunch of
`docker build` commands only anyway. Maybe you use Nix.
Maybe you'll use some sandbox. `SelfCI` leaves it to you.

While we're considering the security - note that the CI
runs the command from the base, not the candidate. This
way the base revision, which is implicitly considered
the vetted and trusted one is in control of the CI.

### Future

After the local-first approach is validated, the feature-set
can be expanded.

For a project that is very active, with many contributors,
it makes sense to easily run a server-side instance of
`selfci` that would automatically check, merge and publish
updated trunk branch.

To facilitate that I'm planning to use [Iroh](https://iroh.network/) P2P
networking and some cryptography. It should also be possible
to pool computers of multiple maintainers to handle running
the CI runs.

All of this, leaving the simple yet flexible core local-first
model.
