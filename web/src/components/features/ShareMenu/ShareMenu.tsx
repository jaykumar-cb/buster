'use client';

import React, { PropsWithChildren, useState } from 'react';
import { Popover } from '@/components/ui/popover/Popover';
import { AppTooltip } from '@/components/ui/tooltip';
import { useMemoizedFn } from '@/hooks';
import { BusterShare, ShareAssetType } from '@/api/asset_interfaces';
import { ShareMenuContent } from './ShareMenuContent';
import { canShare } from '@/lib/share';

export const ShareMenu: React.FC<
  PropsWithChildren<{
    shareAssetConfig: BusterShare | null;
    assetId: string;
    assetType: ShareAssetType;
  }>
> = React.memo(({ children, shareAssetConfig, assetId, assetType }) => {
  const [isOpen, setIsOpen] = useState(false);

  const onOpenChange = useMemoizedFn((v: boolean) => {
    setIsOpen(v);
  });

  const showShareMenu = canShare(shareAssetConfig?.permission);

  if (!showShareMenu) {
    return null;
  }

  return (
    <Popover
      size={'none'}
      onOpenChange={onOpenChange}
      align={'end'}
      side={'bottom'}
      content={
        shareAssetConfig ? (
          <ShareMenuContent
            shareAssetConfig={shareAssetConfig}
            assetId={assetId}
            assetType={assetType}
          />
        ) : null
      }>
      <AppTooltip title={!isOpen ? 'Share item' : ''}>
        <div className="flex">{children}</div>
      </AppTooltip>
    </Popover>
  );
});
ShareMenu.displayName = 'ShareMenu';
