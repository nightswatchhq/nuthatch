# The authoring-eval subject image (#1058)

`eval/run-authoring.py --docker-image <tag>` needs an image to put the subject in. Without one the
runner refuses to start, which is why RFC-0017's authoring eval has a proven board, a runner, two
isolation modes and **no baseline**.

```sh
docker build -t nuthatch-eval-subject eval/image
python3 eval/run-authoring.py --docker-image nuthatch-eval-subject --runs 3 \
        --nuthatch target/release/nuthatch
```

## What the image is and is not responsible for

**Not isolation.** The runner builds the `docker run` itself: only the workdir is mounted, at its own
path, on a `--internal` network. The repository is unreachable *by construction* and there is no route
to the internet. An image cannot weaken that and does not need to strengthen it.

**Usability.** A confinement the subject cannot work in produces a **false zero** - a number that
looks like a failing agent and is really a broken environment. So the image owes `claude`, an HTTP
client, and a shell, and the runner preflights all three before a single score is recorded.

## Two things deliberately absent

**No `nuthatch` binary.** The subject obtains one as a user would; how well it manages that is part of
what is being measured. Baking one in scores an easier task.

**No network access at runtime.** `npm install` happens at *build* time. The eval itself is offline,
and an agent that could reach the internet could read the nuthatch documentation and score well
without the builder skill teaching it anything.

## Pinning

`CLAUDE_CODE_VERSION` is pinned rather than `@latest`. A score is a number about a *pairing* of model,
prompt and tooling; an eval whose subject changes underneath it cannot be compared with the previous
run. Bump it deliberately and record it alongside the model in the report.
