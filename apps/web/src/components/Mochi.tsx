import { cn } from '@/lib/utils';

export type MochiMood = 'sleeping' | 'thinking' | 'happy' | 'confused' | 'alert';

const INK = '#2b2433';
const stroke = { fill: 'none', stroke: INK, strokeWidth: 3, strokeLinecap: 'round', strokeLinejoin: 'round' } as const;

function Face({ mood }: { mood: MochiMood }) {
  switch (mood) {
    case 'sleeping':
      return (
        <>
          <path d="M31 53q5 4 10 0M59 53q5 4 10 0" {...stroke} />
          <ellipse cx="50" cy="65" rx="2.6" ry="2" fill={INK} />
          <path d="M76 22h9l-9 10h9" {...stroke} strokeWidth={2.4} />
        </>
      );
    case 'thinking':
      return (
        <>
          <circle cx="37" cy="51" r="4" fill={INK} />
          <circle cx="63" cy="51" r="4" fill={INK} />
          <circle cx="38.5" cy="49.5" r="1.3" fill="#fff" />
          <circle cx="64.5" cy="49.5" r="1.3" fill="#fff" />
          <path d="M44 65h12" {...stroke} />
          <circle cx="79" cy="30" r="2.2" fill={INK} />
          <circle cx="85" cy="22" r="3" fill={INK} />
          <circle cx="93" cy="13" r="4" fill={INK} />
        </>
      );
    case 'happy':
      return (
        <>
          <path d="M31 54q5.5-7 11 0M58 54q5.5-7 11 0" {...stroke} />
          <path d="M42 61q8 10 16 0Z" fill={INK} stroke={INK} strokeWidth={2} strokeLinejoin="round" />
          <path d="M84 24l2 5 5 2-5 2-2 5-2-5-5-2 5-2Z" fill="#efc56b" />
        </>
      );
    case 'confused':
      return (
        <>
          <circle cx="37" cy="51" r="4" fill={INK} />
          <path d="M58 47l9 8M67 47l-9 8" {...stroke} />
          <path d="M42 66q4-4 8 0t8 0" {...stroke} />
          <path d="M80 18q7-6 11 0t-6 9v3" {...stroke} strokeWidth={2.6} />
          <circle cx="85" cy="36" r="1.7" fill={INK} />
        </>
      );
    case 'alert':
      return (
        <>
          <circle cx="37" cy="51" r="5" fill="#fff" stroke={INK} strokeWidth={2.5} />
          <circle cx="63" cy="51" r="5" fill="#fff" stroke={INK} strokeWidth={2.5} />
          <circle cx="37" cy="51" r="2" fill={INK} />
          <circle cx="63" cy="51" r="2" fill={INK} />
          <ellipse cx="50" cy="66" rx="4" ry="5" fill={INK} />
          <path d="M87 14v14" {...stroke} stroke="#e4572e" strokeWidth={4} />
          <circle cx="87" cy="36" r="2.4" fill="#e4572e" />
        </>
      );
  }
}

export const MOOD_LABEL: Record<MochiMood, string> = {
  sleeping: 'Agents paused',
  thinking: 'Analyzing the market',
  happy: 'Trade executed',
  confused: 'AI error',
  alert: 'Risk warning',
};

export function Mochi({ mood, size = 64, className }: { mood: MochiMood; size?: number; className?: string }) {
  return (
    <svg
      viewBox="0 0 100 92"
      width={size}
      height={size * 0.92}
      role="img"
      aria-label={`Mochi: ${MOOD_LABEL[mood]}`}
      className={cn('shrink-0 overflow-visible', className)}
    >
      <g className={`mochi-${mood}`}>
        <ellipse cx="50" cy="84" rx="34" ry="4" fill="#000" opacity="0.14" />
        <path d="M50 18c23 0 40 17 40 38 0 19-17 28-40 28S10 75 10 56c0-21 17-38 40-38Z" fill="#fdf1e6" stroke="#ecd6c2" strokeWidth="2" />
        <path d="M24 34q8-9 20-11" fill="none" stroke="#fff" strokeWidth="4" strokeLinecap="round" />
        <ellipse cx="24" cy="62" rx="6.5" ry="4" fill="#f6b3c8" />
        <ellipse cx="76" cy="62" rx="6.5" ry="4" fill="#f6b3c8" />
        <Face mood={mood} />
      </g>
    </svg>
  );
}
