# SelfCI

SelfCI is a minimalistic local-first Unix-philosophy-abiding CI.

### Status

Just started hacking on it recently, and all details can still change.

Feel free to join [`#support:dpc.pw` Matrix channel](https://matrix.to/#/#support:dpc.pw)
and I'm happy to hear your feedback and thoughts on the matter.

### Philosophy of the design

SelfCI is a **Continuous Integration** system. It
performs the same role platforms like GitHub Actions, Circle CI, etc. typically
do: make sure you can make changes and integrate contributions to your codebase
frequently and with confidence, However, it approaches the problem completely differently.

The core observation behind the SelfCI's design is
that small to medium sized projects could do just fine
if the maintainers and developers just ran the CI locally
themselves.

Most developers are able to compile the source code their
work on, run tests, etc. The reproducibility and isolation are
more (Nix) or less (Docker) a solved problem too.

Why do we then remain at the mercy of big corporations
to be allowed to run CI on a fleet of super slower VMs and/or
complicate everything setting up servers?

Do you really so desperately need a cloud or a 24/7 available server
to run something you and your contributors can just do yourself?

SelfCI is **local-first**. It is meant to be used primarily locally,
on developers machines, in a same way tools like `git` are used.
The design can support "scaling out" and setting up dedicated
servers, but notably does not _require_ it. Setting up CI with
`selfci init` is no different than setting up Git repository
with `git init` or package management with `cargo init`, etc.

SelfCI is implemented as a single `selfci` command,
and behaves like a typical Unix tool. It
implements carefully thought through minimal set of features
needed to compose with any other Unix software to allow defining
CI rules however you want.


### Notable features

* Any scripting or programming language can be used to implement the CI rules.
* Built-in parallel "jobs" that can be started dynamically at any point,
  using arbitrary conditions.
* Built-in "steps" of a "job" with potentially non-blocking failures.
* Local Merge Queue daemon.
* Flexible security model.
* Bring-your-own-isolation.


### SelfCI CI rules

The core concepts in SelfCI are the "base" and the "candidate".

The "base" is a source code version that is known to work.
Think `main`/`master` branch. The candidate is the code version with
proposed changes that need to be verified. Think GitHub PR.

The CI run is validating "candidate" against the "base". It is
a subtle but important security aspect of the design:
the "base" (which is considered vetted and trusted) defines the root
of "rules" that need to met before the "candidate" "passes the CI".
More on it later.

To initialize the CI run `selfci init` in the root directory of
your project's source code. This will create a `.config/selfci/ci.yaml` file
with a template to customize. Yes, I know it's yaml, and yaml ... is meh.
But it is a tiny yaml file which allows you to quickly delegate all the actual
logic to something better and more flexible.

All CI rules are implemented through the execution
of a single script/command - later called "candidate check".

You can view the [ci.yaml](./.config/selfci/ci.yaml) and
[ci.sh](./.config/selfci/ci.sh) which SelfCI itself
uses for a quick example.

When the CI run starts the CI check command _from the base source_,
is executed as the "main" job. The `SELFCI_JOB_NAME` environment variable is set to `main`,
`SELFCI_CANDIDATE_DIR` will point at and the current working will be set to
a directory which contains a temporary copy of the candidate
and `SELFCI_BASE_DIR` will point at a temporary copy of the base.

In SelfCI the parallel units of execution
are called "jobs" and can be started dynamically
from within existing jobs using `selfci job start <name>` at
any time. Each job will start the same single command, but with a different
`SELFCI_JOB_NAME` each time. Any job can wait for another job to finish
using `selfci job wait <name>`.

This way you are in a full control of what jobs do or do
not run, and you don't need to define them upfront
using clumsy YAML DSLs.

The `selfci` command is used as the API to control the execution
of the CI rules. You don't need to call it from a bash
script. If your CI rules are complex enough, consider
building and executing a binary that calls `selfci` internally.
Although the built-in parallel execution of jobs should make
implementing most CI rules very manageable as a single bash script.

`selfci step start <name>` informs the runner that
a new step of the current job begins, allowing it to track
execution times and outcomes of individual parts
of each job. Each step is considered successful,
unless `selfci step fail [--ignore]` is called.
`--ignore` flag makes the job as a whole not
be considered as failed, while still giving user
the failure feedback.

A CI run has passed if all the jobs it started
were successful. Each job is successful if
it returned a zero exit code, and no step
was marked as failed (without the `--ignore` flag).

### `selfci check` & `selfci mq`

You can easily start a CI run with an
arbitrary base and candidate using `selfci check`.
This is an easy way to run the CI locally to verify change,
and is mostly meant for developers/contributors to self-validate
their changes, just like a typical CI would.

For maintainers a suite of `selfci mq` commands is
implemented to act more like GitHub Action's Merge Queue system.

Project maintainer can start a local CI daemon,
and after reviewing submitted contributors, add
them to the SelfCI's Merge Queue.

The mq daemon will run the candidate check against the base branch
and merge passing changes into the base branch. This allows conveniently merging changes
without the need to babysit and wait for things to finish.


### "Bring your own isolation" and security

GitHub Actions executes every job as a separate
virtual machine that typically makes a full copy of the source code
over the network. This ensures that multiple parallel jobs
don't interfere with each other, malicious code does not affect
host system and that each job can potentially run on a different server
in the cloud.

This is the most conservative choice, necessary for public consumption.
The downside of it is that it is extremely wasteful.

Most (all?) alternative CI systems tend to copy this model and
contain a built-in isolation systems with separate servers, sandboxes,
virtual machines, etc.

Baking in virtual machines or containers, etc. does not
make sense for a local-first CI. It makes things more complicated,
harder to set up, less composable, heavier and slower.

`selfci` leaves it to the user and developer implementing the CI to take care
of job isolation in both senses: protecting host system from malicious code and protecting
CI jobs running in parallel from interfering.

I don't know about you, but I typically
collaborate with people that I know and trust, running
code they wrote on a daily basis. I do not even think
about merging code that I did not review, especially
from strangers.

Using SelfCI in combination with e.g. Nix any developer is able to run
the exact same reproducible CI on their own machine.

Why would I need a server running 24h to do it for them,
that I need to worry about maintaining and protecting from risk
of running unreviewed malicious code?

As you implement your CI you need to consider your specific
needs. Maybe you're going to run bunch of
`docker build` commands only anyway. Maybe you use Nix.
Maybe you'll use some sandbox or a VM. `SelfCI` leaves it to you.

An important part of the isolation/security story
is that SelfCI starts with the command defined by the base
(which is considered vetted and trusted) and
not the candidate change. This makes it possible
to implement rules and checks that can't be changed
by the submitted code. It's a subtle but important
aspect of the design. One could e.g. have checks in the base
forbidding changes to certain files, or cryptographic
signatures, etc. allowing building more automated, yet
secure CI policies with full flexibility.


### Future: "scaling-out"

After the local-first approach is validated, the feature-set
can be expanded.

For a project that is very active, with many contributors,
and potentially a heavy CI, it makes sense to easily run a server-side
instance of `selfci` that would automatically check, merge and publish
updated trunk branch. It would also make sense to pool the resources
of multiple maintainers to help utilize their hardware better
and save time waiting.

To facilitate that I'm planning to use [Iroh](https://iroh.network/) P2P
networking and some cryptography.

All of this should be possible while preserving the simple yet flexible core local-first
model.
