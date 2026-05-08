/// <reference types="vite/client" />

// Ambient declaration for side-effect CSS imports.
// Vite handles these at build time; tsc 6+ needs an explicit declaration.
declare module '*.css';
