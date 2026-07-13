import React from 'react';
import { Composition } from 'remotion';
import { DejaDemo, TOTAL } from './DejaDemo';
import { ShotGraph, ShotMemories } from './Shots';

export const RemotionRoot: React.FC = () => (
  <>
    <Composition id="DejaDemo" component={DejaDemo} durationInFrames={TOTAL} fps={30} width={1920} height={1080} />
    <Composition id="ShotGraph" component={ShotGraph} durationInFrames={30} fps={30} width={1920} height={1080} />
    <Composition id="ShotMemories" component={ShotMemories} durationInFrames={30} fps={30} width={1920} height={1080} />
  </>
);
