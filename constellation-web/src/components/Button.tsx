// CONST-17: Button primitive per §2.3 — 4 variants (primary/secondary/ghost/danger) x
// 3 sizes (sm/md/lg). Backed by the existing `.h-btn*` classes in globals.css.
export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
export type ButtonSize = 'sm' | 'md' | 'lg';

const VARIANT_CLASS: Record<ButtonVariant, string> = {
  primary: 'h-btn-primary',
  secondary: 'h-btn-secondary',
  ghost: 'h-btn-ghost',
  danger: 'h-btn-danger',
};

const SIZE_CLASS: Record<ButtonSize, string> = {
  sm: 'h-btn-sm',
  md: 'h-btn-md',
  lg: 'h-btn-lg',
};

interface ButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
}

export function Button({ variant = 'secondary', size = 'md', className, children, ...rest }: ButtonProps) {
  return (
    <button
      className={`h-btn ${VARIANT_CLASS[variant]} ${SIZE_CLASS[size]}${className ? ` ${className}` : ''}`}
      {...rest}
    >
      {children}
    </button>
  );
}
