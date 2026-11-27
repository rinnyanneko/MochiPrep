import { cva, type VariantProps } from 'class-variance-authority';
import { X } from 'lucide-react';
import { Dialog as RadixDialog, Tabs as RadixTabs } from 'radix-ui';
import type { ButtonHTMLAttributes, HTMLAttributes, InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';

const buttonVariants = cva(
  'inline-flex items-center justify-center gap-1.5 rounded-md font-medium whitespace-nowrap transition-colors duration-100 disabled:pointer-events-none disabled:opacity-40 [&_svg]:size-4 [&_svg]:shrink-0',
  {
    variants: {
      variant: {
        primary: 'bg-accent text-accent-ink hover:opacity-90',
        secondary: 'border border-line bg-surface text-ink hover:bg-surface-2',
        ghost: 'text-ink-2 hover:bg-surface-2 hover:text-ink',
        danger: 'border border-line bg-surface text-down hover:bg-down-soft',
        long: 'bg-rise text-[color:var(--rise-ink,var(--bg))] hover:opacity-90',
        short: 'bg-fall text-[color:var(--fall-ink,var(--bg))] hover:opacity-90',
      },
      size: { sm: 'h-7 px-2.5 text-xs', md: 'h-8 px-3 text-[13px]', lg: 'h-10 px-4 text-sm', icon: 'size-8' },
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
    <div className="flex items-start justify-between gap-3 px-4 pt-3 pb-2.5">
      <div className="min-w-0">
        <h2 className="text-[13px] font-semibold">{title}</h2>
        {description && <p className="mt-0.5 text-xs text-ink-3">{description}</p>}
      </div>
      {action}
    </div>
  );
}

const fieldClass =
  'h-8 w-full rounded-md border border-line bg-surface px-2.5 text-[13px] text-ink placeholder:text-ink-3 focus:border-ink-3 focus:outline-none disabled:opacity-50';

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
      <span className="mb-1 block text-xs font-medium text-ink-2">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-[11px] text-ink-3">{hint}</span>}
    </label>
  );
}

const badgeTones = {
  neutral: 'bg-surface-2 text-ink-2',
  up: 'bg-up-soft text-up',
  down: 'bg-down-soft text-down',
  /** Market direction. Unlike up and down, these swap when the user flips colors. */
  rise: 'bg-rise-soft text-rise',
  fall: 'bg-fall-soft text-fall',
  warn: 'bg-warn-soft text-warn',
  info: 'bg-info-soft text-info',
  accent: 'bg-accent-soft text-ink',
};
export type Tone = keyof typeof badgeTones;

export function Badge({ tone = 'neutral', className, ...props }: HTMLAttributes<HTMLSpanElement> & { tone?: Tone }) {
  return (
    <span
      className={cn('inline-flex items-center gap-1 rounded px-1.5 py-px text-[11px] font-medium whitespace-nowrap [&_svg]:size-3', badgeTones[tone], className)}
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
        <RadixDialog.Overlay className="fixed inset-0 z-40 bg-black/60" />
        <RadixDialog.Content
          aria-describedby={undefined}
          className={cn(
            'fixed top-1/2 left-1/2 z-50 max-h-[88vh] w-[calc(100vw-32px)] -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-[var(--radius-card)] border border-line bg-surface p-5',
            wide ? 'max-w-2xl' : 'max-w-md',
          )}
        >
          <div className="mb-4 flex items-start justify-between gap-3">
            <div>
              <RadixDialog.Title className="text-base font-semibold">{title}</RadixDialog.Title>
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
      <RadixTabs.List className="flex gap-4 overflow-x-auto border-b border-line px-4">
        {tabs.map((tab) => (
          <RadixTabs.Trigger
            key={tab.id}
            value={tab.id}
            className="-mb-px border-b-2 border-transparent py-2.5 text-[13px] font-medium whitespace-nowrap text-ink-3 hover:text-ink data-[state=active]:border-ink data-[state=active]:text-ink"
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
    <div role="group" aria-label={label} className="inline-flex rounded-md border border-line bg-surface-2 p-0.5">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
          className={cn(
            'h-6 rounded px-2.5 text-xs font-medium whitespace-nowrap transition-colors duration-100',
            value === option.value ? 'bg-surface text-ink shadow-[0_0_0_1px_var(--line)]' : 'text-ink-3 hover:text-ink',
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
          <tr className="text-left text-xs text-ink-3">
            {head.map((h, i) => (
              <th key={i} className="px-3 py-2 font-normal whitespace-nowrap first:pl-4 last:pr-4">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="[&_td]:border-t [&_td]:border-line [&_td]:px-3 [&_td]:py-2 [&_td]:whitespace-nowrap [&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_tr:hover_td]:bg-surface-2">
          {children}
        </tbody>
      </table>
      {empty && <p className="px-4 py-8 text-center text-[13px] text-ink-3">{empty}</p>}
    </div>
  );
}

/** One figure in a [`StatStrip`]: label above, value, optional note below. */
export function Stat({ label, value, sub, className }: { label: string; value: ReactNode; sub?: ReactNode; className?: string }) {
  return (
    <div className={cn('min-w-0 px-4 py-3', className)}>
      <div className="text-xs text-ink-3">{label}</div>
      <div className="num mt-1 truncate text-lg font-semibold">{value}</div>
      {sub && <div className="num mt-0.5 truncate text-[11px] text-ink-3">{sub}</div>}
    </div>
  );
}

/** A row of figures in one panel, divided by hairlines rather than boxed one by one. */
export function StatStrip({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <Card className={cn('grid divide-line max-md:divide-y md:divide-x', className)}>
      {children}
    </Card>
  );
}
