// The plan documents the new-campaign page offers to start from, bundled as written.

const FILES = import.meta.glob<string>("./plan-examples/*.toml", { query: "?raw", import: "default", eager: true });

/** Each example's file stem and title, in the order the page offers them. */
export const TITLES = {
  "mlx-macos": "MLX on a Mac",
  "llamacpp-macos": "llama.cpp on a Mac",
  "vllm-ubuntu-gpu-evals": "vLLM evals on an Ubuntu GPU host",
  "afm-iphone": "Apple Foundation on an iPhone",
} as const;

export interface PlanExample {
  id: string;
  title: string;
  text: string;
}

export const PLAN_EXAMPLES: readonly PlanExample[] = Object.entries(TITLES).map(([id, title]) => ({
  id, title, text: FILES[`./plan-examples/${id}.toml`] ?? "",
}));
