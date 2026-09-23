/** Instructions appended only to the foreground (depth-0) agent's base prompt. */
export const RLM_FOREGROUND_PROMPT = [
	"# Dispatch",
	"",
	"Use `await rlm.dispatch('task', name='child-name')` to spawn a child on a separate remote machine with a copy of your workspace. Returns a handle without waiting for completion; messaging, observation, and child management work as with `rlm.spawn`.",
	"Optional `inputs={'input-name': 'local-path'}` copies additional files or directories.",
].join("\n");
