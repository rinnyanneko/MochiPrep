import { Send } from 'lucide-react';
import { useState, type FormEvent } from 'react';
import { Button, Input } from '@/components/ui';
import { api } from '@/lib/api';
import { MESSAGE_MAX, messageNote, type MessageReply } from '@/lib/agent-message';
import { attempt } from '@/store';

/**
 * A message for the agent's next decision, which runs at once: a correction such as "funding
 * flipped, stay flat". The model reads it like the strategy notes, for that one decision only;
 * it cannot change the risk limits. `send` wraps the request, so the caller can arm first.
 */
export function AgentMessage({ agentId, agentName, send, onSent }: Readonly<{ agentId: string; agentName: string; send(run: () => void): void; onSent(): void }>) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const message = text.trim();
    if (!message) return;
    send(() => {
      void (async () => {
        setBusy(true);
        setNote(null);
        const reply = await attempt(() => api.post<MessageReply>(`/agents/${agentId}/message`, { text: message }));
        setBusy(false);
        if (!reply) return;
        setText('');
        setNote(messageNote(reply));
        onSent();
      })();
    });
  };

  return (
    <form className="mt-3" onSubmit={submit} data-testid="agent-message">
      <div className="flex gap-1.5">
        <Input
          value={text}
          maxLength={MESSAGE_MAX}
          onChange={(e) => setText(e.target.value)}
          placeholder="Tell the agent something for its next decision..."
          aria-label={`Message to ${agentName}`}
          disabled={busy}
        />
        <Button size="sm" type="submit" disabled={busy || !text.trim()} title="Runs one cycle now with this message"><Send /> Send</Button>
      </div>
      <p className="mt-1 text-[11px] text-ink-3">{note ?? 'Runs one cycle now. Used for that decision only; the risk limits still apply.'}</p>
    </form>
  );
}
