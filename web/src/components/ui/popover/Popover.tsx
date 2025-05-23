import {
  PopoverRoot as PopoverBase,
  PopoverContent,
  PopoverContentVariant,
  PopoverTrigger,
  PopoverTriggerType
} from './PopoverBase';
import React from 'react';

export interface PopoverProps
  extends React.ComponentProps<typeof PopoverBase>,
    Pick<React.ComponentProps<typeof PopoverContent>, 'align' | 'side'> {
  children: React.ReactNode;
  content: React.ReactNode;
  className?: string;
  trigger?: PopoverTriggerType;
  size?: PopoverContentVariant['size'];
  sideOffset?: number;
}

export const Popover = React.memo<PopoverProps>(
  ({
    children,
    content,
    align,
    side,
    className = '',
    trigger = 'click',
    size = 'default',
    sideOffset,
    ...props
  }) => {
    return (
      <PopoverBase trigger={trigger} {...props}>
        <PopoverTrigger asChild>
          <span className="">{children}</span>
        </PopoverTrigger>
        <PopoverContent
          align={align}
          side={side}
          className={className}
          size={size}
          sideOffset={sideOffset}>
          {content}
        </PopoverContent>
      </PopoverBase>
    );
  }
);

Popover.displayName = 'Popover';
