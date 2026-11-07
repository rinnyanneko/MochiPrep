import { cva, type VariantProps } from 'class-variance-authority';
import { X } from 'lucide-react';
import { Dialog as RadixDialog, Tabs as RadixTabs } from 'radix-ui';
import type { ButtonHTMLAttributes, HTMLAttributes, InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';

const buttonVariants = cva(
  'inline-flex items-center justify-center gap-1.5 rounded-xl font-semibold whitespace-nowrap transition-colors disabled:pointer-events-none disabled:opacity-45 [&_svg]:size-4 [&_svg]:shrink-0',
  {
    variants: {
      variant: {
        primary: 'bg-accent text-accent-ink hover:opacity-90',
        secondary: 'bg-surface-2 text-ink hover:bg-surface-3',
        ghost: 'text-ink-2 hover:bg-surface-2 hover:text-ink',
        danger: 'bg-down-soft text-down hover:opacity-80',
        long: 'bg-up text-bg hover:opacity-90',
        short: 'bg-down text-bg hover:opacity-90',
      },
      size: { sm: 'h-8 px-2.5 text-xs', md: 'h-9 px-3.5 text-[13px]', lg: 'h-11 px-5 text-sm', icon: 'size-8' },
    },
    defaultVariants: { variant: 'secondary', size: 'md' },
  },
);

export function Button({
  className,
  variant,
  size,
  type = 'button',
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & VariantProps<typeof buttonVariants>) {
  return <button type={type} className={cn(buttonVariants({ variant, size }), className)} {...props} />;
}

export function Card({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn('rounded-[var(--radius-card)] border border-line bg-surface', className)} {...props} />;
}

export function CardHeader({ title, description, action }: { title: ReactNode; description?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-3 px-5 pt-4 pb-3">
      <div className="min-w-0">
        <h2 className="font-display text-[15px] font-extrabold">{title}</h2>
        {description && <p className="mt-0.5 text-xs text-ink-3">{description}</p>}
      </div>
      {action}
    </div>
  );
}

const fieldClass =
  'h-9 w-full rounded-xl border border-line bg-surface-2 px-3 text-[13px] text-ink placeholder:text-ink-3 focus:border-accent focus:outline-none disabled:opacity-50';

export function Input({ className, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  return <input className={cn(fieldClass, className)} {...props} />;
}

export function Textarea({ className, ...props }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea className={cn(fieldClass, 'h-24 resize-y py-2', className)} {...props} />;
}

export function Select({ className, ...props }: SelectHTMLAttributes<HTMLSelectElement>) {
  return <select className={cn(fieldClass, 'pr-7', className)} {...props} />;
}

export function Field({ label, hint, children, className }: { label: string; hint?: string; children: ReactNode; className?: string }) {
  return (
    <label className={cn('block', className)}>
      <span className="mb-1 block text-xs font-semibold text-ink-2">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-[11px] text-ink-3">{hint}</span>}
    </label>
  );
}

const badgeTones = {
  neutral: 'bg-surface-2 text-ink-2',
  up: 'bg-up-soft text-up',
  down: 'bg-down-soft text-down',
  warn: 'bg-warn-soft text-warn',
  info: 'bg-info-soft text-info',
  accent: 'bg-accent-soft text-accent',
};
export type Tone = keyof typeof badgeTones;

export function Badge({ tone = 'neutral', className, ...props }: HTMLAttributes<HTMLSpanElement> & { tone?: Tone }) {
  return (
    <span
      className={cn('inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-semibold whitespace-nowrap [&_svg]:size-3', badgeTones[tone], className)}
      {...props}
    />
  );
}

export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  wide,
}: {
  open: boolean;
  onOpenChange(open: boolean): void;
  title: string;
  description?: string;
  children: ReactNode;
  wide?: boolean;
}) {
  return (
    <RadixDialog.Root open={open} onOpenChange={onOpenChange}>
      <RadixDialog.Portal>
        <RadixDialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <RadixDialog.Content
          aria-describedby={undefined}
          className={cn(
            'fixed top-1/2 left-1/2 z-50 max-h-[88vh] w-[calc(100vw-32px)] -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-[var(--radius-card)] border border-line bg-surface p-5',
            wide ? 'max-w-2xl' : 'max-w-md',
          )}
        >
          <div className="mb-4 flex items-start justify-between gap-3">
            <div>
              <RadixDialog.Title className="font-display text-lg font-extrabold">{title}</RadixDialog.Title>
              {description && <RadixDialog.Description className="mt-0.5 text-xs text-ink-3">{description}</RadixDialog.Description>}
            </div>
            <RadixDialog.Close asChild>
              <Button variant="ghost" size="icon" aria-label="Close">
                <X />
              </Button>
            </RadixDialog.Close>
          </div>
          {children}
        </RadixDialog.Content>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}

export function Tabs({
  tabs,
  value,
  onChange,
  children,
}: {
  tabs: { id: string; label: ReactNode }[];
  value: string;
  onChange(id: string): void;
  children: ReactNode;
}) {
  return (
    <RadixTabs.Root value={value} onValueChange={onChange}>
      <RadixTabs.List className="flex gap-1 overflow-x-auto border-b border-line px-3">
        {tabs.map((tab) => (
          <RadixTabs.Trigger
            key={tab.id}
            value={tab.id}
            className="-mb-px border-b-2 border-transparent px-3 py-2.5 text-[13px] font-semibold whitespace-nowrap text-ink-3 hover:text-ink data-[state=active]:border-accent data-[state=active]:text-ink"
          >
            {tab.label}
          </RadixTabs.Trigger>
        ))}
      </RadixTabs.List>
      <RadixTabs.Content value={value} tabIndex={-1}>
        {children}
      </RadixTabs.Content>
    </RadixTabs.Root>
  );
}

export function Segmented<T extends string>({
  options,
  value,
  onChange,
  label,
}: {
  options: { value: NoInfer<T>; label: ReactNode }[];
  value: T;
  onChange: (value: NoInfer<T>) => void;
  label: string;
}) {
  return (
    <div role="group" aria-label={label} className="inline-flex rounded-xl bg-surface-2 p-0.5">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
          className={cn(
            'h-7 rounded-[10px] px-2.5 text-xs font-semibold whitespace-nowrap transition-colors',
            value === option.value ? 'bg-surface-3 text-ink' : 'text-ink-3 hover:text-ink',
          )}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function Table({ head, children, empty }: { head: ReactNode[]; children: ReactNode; empty?: string | false }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-[13px]">
        <thead>
          <tr className="text-left text-[11px] tracking-wide text-ink-3 uppercase">
            {head.map((h, i) => (
              <th key={i} className="px-4 py-2 font-semibold whitespace-nowrap first:pl-5 last:pr-5">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="[&_td]:border-t [&_td]:border-line [&_td]:px-4 [&_td]:py-2.5 [&_td]:whitespace-nowrap [&_td:first-child]:pl-5 [&_td:last-child]:pr-5">
          {children}
        </tbody>
      </table>
      {empty && <p className="px-5 py-8 text-center text-[13px] text-ink-3">{empty}</p>}
    </div>
  );
}

export function Stat({ label, value, sub, className }: { label: string; value: ReactNode; sub?: ReactNode; className?: string }) {
  return (
    <Card className={cn('px-4 py-3.5', className)}>
      <div className="text-xs text-ink-3">{label}</div>
      <div className="num mt-1 text-xl font-semibold">{value}</div>
      {sub && <div className="mt-0.5 text-[11px] text-ink-3">{sub}</div>}
    </Card>
  );
}
