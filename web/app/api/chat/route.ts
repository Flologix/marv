import { APPROVE_SCOPES, type ApproveScope, isChatId, isModelId, isRunning, runTurn } from "@/lib/marv";

export async function POST(request: Request) {
  const body = await request.json().catch(() => null);
  const prompt = typeof body?.prompt === "string" ? body.prompt.trim() : "";
  if (!isChatId(body?.chatId) || !prompt || !isModelId(body?.model)) {
    return Response.json({ error: "Expected chatId, prompt and model." }, { status: 400 });
  }
  if (isRunning(body.chatId)) {
    return Response.json({ error: "This chat is already running a turn." }, { status: 409 });
  }
  const approve = (Array.isArray(body.approve) ? body.approve : []).filter((s: unknown): s is ApproveScope =>
    (APPROVE_SCOPES as readonly unknown[]).includes(s),
  );
  const maxSteps = Number.isInteger(body.maxSteps) && body.maxSteps >= 0 && body.maxSteps <= 500 ? body.maxSteps : 50;

  try {
    const stream = runTurn(
      { chatId: body.chatId, prompt, model: body.model, approve, maxSteps, resume: body.resume === true },
      request.signal,
    );
    return new Response(stream, {
      headers: { "Content-Type": "application/x-ndjson; charset=utf-8", "Cache-Control": "no-store" },
    });
  } catch (error) {
    return Response.json({ error: (error as Error).message }, { status: 500 });
  }
}
