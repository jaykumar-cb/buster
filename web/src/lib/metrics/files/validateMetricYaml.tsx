import React, { useRef, useEffect } from 'react';
import * as yaml from 'js-yaml';
import * as monaco from 'monaco-editor';
import { type editor } from 'monaco-editor/esm/vs/editor/editor.api';
import MonacoEditor from '@monaco-editor/react';

type IMarkerData = editor.IMarkerData;

// Initial YAML content for testing
const initialValue = `Person: "John Doe"
Place: "Wonderland"
Age: 30
Siblings:
  Jane: 25
  Jim: 28`;

// Helper function to find line number for a specific key in YAML content
const findLineNumberForKey = (content: string, key: string): number => {
  const lines = content.split('\n');
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trim().startsWith(`${key}:`)) {
      return i + 1; // Line numbers are 1-based
    }
  }
  return 1; // Default to line 1 if not found
};

// Linting function that validates YAML content
export const validateMetricYaml = (
  content: string,
  monaco: typeof import('monaco-editor')
): IMarkerData[] => {
  const markers: IMarkerData[] = [];
  let parsed: any;

  // Parse YAML content
  try {
    parsed = yaml.load(content);
  } catch (error: any) {
    // For parse errors, extract line and column information from the error
    let lineNumber = 1;
    let columnNumber = 1;

    // js-yaml errors include mark object with position information
    if (error.mark) {
      lineNumber = error.mark.line + 1; // Convert to 1-based line numbering
      columnNumber = error.mark.column + 1; // Convert to 1-based column numbering
    } else {
      // Fallback to regex for older versions or different error types
      const lineMatch = error.message.match(/line (\d+)/i);
      if (lineMatch && lineMatch[1]) {
        lineNumber = parseInt(lineMatch[1], 10);
      }
    }

    markers.push({
      severity: monaco.MarkerSeverity.Error,
      message: 'Invalid YAML: ' + error.message,
      startLineNumber: lineNumber,
      startColumn: columnNumber,
      endLineNumber: lineNumber,
      endColumn: columnNumber + 1 // Highlight at least one character
    });
    return markers;
  }

  // Ensure the parsed result is an object
  if (typeof parsed !== 'object' || parsed === null) {
    markers.push({
      severity: monaco.MarkerSeverity.Error,
      message: 'YAML content should be an object with properties.',
      startLineNumber: 1,
      startColumn: 1,
      endLineNumber: 1,
      endColumn: 1
    });
    return markers;
  }

  // Allowed keys
  const allowedKeys = ['Person', 'Place', 'Age', 'Siblings'];
  const keys = Object.keys(parsed);

  // Check for any unexpected keys
  keys.forEach((key) => {
    if (!allowedKeys.includes(key)) {
      const lineNumber = findLineNumberForKey(content, key);
      markers.push({
        severity: monaco.MarkerSeverity.Error,
        message: `Unexpected key "${key}". Only ${allowedKeys.join(', ')} are allowed.`,
        startLineNumber: lineNumber,
        startColumn: 1,
        endLineNumber: lineNumber,
        endColumn: content.split('\n')[lineNumber - 1]?.length || 1
      });
    }
  });

  // Check for missing keys
  allowedKeys.forEach((key) => {
    if (!(key in parsed)) {
      markers.push({
        severity: monaco.MarkerSeverity.Error,
        message: `Missing required key "${key}".`,
        startLineNumber: 1,
        startColumn: 1,
        endLineNumber: 1,
        endColumn: 1
      });
    }
  });

  // Validate types for each field
  if ('Person' in parsed && typeof parsed.Person !== 'string') {
    const lineNumber = findLineNumberForKey(content, 'Person');
    markers.push({
      severity: monaco.MarkerSeverity.Error,
      message: 'The "Person" field must be a string.',
      startLineNumber: lineNumber,
      startColumn: 1,
      endLineNumber: lineNumber,
      endColumn: content.split('\n')[lineNumber - 1]?.length || 1
    });
  }

  if ('Place' in parsed && typeof parsed.Place !== 'string') {
    const lineNumber = findLineNumberForKey(content, 'Place');
    markers.push({
      severity: monaco.MarkerSeverity.Error,
      message: 'The "Place" field must be a string.',
      startLineNumber: lineNumber,
      startColumn: 1,
      endLineNumber: lineNumber,
      endColumn: content.split('\n')[lineNumber - 1]?.length || 1
    });
  }

  if ('Age' in parsed && typeof parsed.Age !== 'number') {
    const lineNumber = findLineNumberForKey(content, 'Age');
    markers.push({
      severity: monaco.MarkerSeverity.Error,
      message: 'The "Age" field must be a number.',
      startLineNumber: lineNumber,
      startColumn: 1,
      endLineNumber: lineNumber,
      endColumn: content.split('\n')[lineNumber - 1]?.length || 1
    });
  }

  if ('Siblings' in parsed) {
    const siblingsLineNumber = findLineNumberForKey(content, 'Siblings');

    if (
      typeof parsed.Siblings !== 'object' ||
      Array.isArray(parsed.Siblings) ||
      parsed.Siblings === null
    ) {
      markers.push({
        severity: monaco.MarkerSeverity.Error,
        message: 'The "Siblings" field must be an object with sibling name-age pairs.',
        startLineNumber: siblingsLineNumber,
        startColumn: 1,
        endLineNumber: siblingsLineNumber,
        endColumn: content.split('\n')[siblingsLineNumber - 1]?.length || 1
      });
    } else {
      // Validate that each sibling's age is a number
      const lines = content.split('\n');
      Object.entries(parsed.Siblings).forEach(([siblingName, siblingAge]) => {
        if (typeof siblingAge !== 'number') {
          // Look for the sibling in the content
          let siblingLineNumber = siblingsLineNumber + 1; // Start after Siblings:
          for (let i = siblingsLineNumber; i < lines.length; i++) {
            if (lines[i].trim().startsWith(`${siblingName}:`)) {
              siblingLineNumber = i + 1;
              break;
            }
          }

          markers.push({
            severity: monaco.MarkerSeverity.Error,
            message: `The age for sibling "${siblingName}" must be a number.`,
            startLineNumber: siblingLineNumber,
            startColumn: 1,
            endLineNumber: siblingLineNumber,
            endColumn: lines[siblingLineNumber - 1]?.length || 1
          });
        }
      });
    }
  }

  return markers;
};

// Helper to configure Monaco YAML schema
const configureYamlSchema = (
  monaco: typeof import('monaco-editor'),
  editor: editor.IStandaloneCodeEditor
) => {
  // This assumes you have monaco-yaml configured via yamlHelper.ts in the project
  // The schema will help with validation and autocomplete
  const model = editor.getModel();
  if (model) {
    // Check if the YAML language support exists
    // This is dynamically added by monaco-yaml and may not be typed correctly
    const yamlDefaults = (monaco.languages as any).yaml?.yamlDefaults;
    if (yamlDefaults && typeof yamlDefaults.setDiagnosticsOptions === 'function') {
      yamlDefaults.setDiagnosticsOptions({
        validate: true,
        schemas: [
          {
            uri: 'http://myserver/metric-yaml-schema.json',
            fileMatch: ['*'],
            schema: {
              type: 'object',
              required: ['Person', 'Place', 'Age', 'Siblings'],
              properties: {
                Person: { type: 'string' },
                Place: { type: 'string' },
                Age: { type: 'number' },
                Siblings: {
                  type: 'object',
                  additionalProperties: { type: 'number' }
                }
              }
            }
          }
        ]
      });
    }
  }
};

export const MyYamlEditor: React.FC = () => {
  const editorRef = useRef<editor.IStandaloneCodeEditor | null>(null);

  // Called once the Monaco editor is mounted
  const editorDidMount = (
    editor: editor.IStandaloneCodeEditor,
    monacoInstance: typeof import('monaco-editor')
  ) => {
    editorRef.current = editor;

    // Try to configure YAML schema if monaco-yaml is properly loaded
    try {
      // Check if the YAML language support exists using type assertion
      if ((monacoInstance.languages as any).yaml?.yamlDefaults) {
        configureYamlSchema(monacoInstance, editor);
      }
    } catch (err) {
      console.error('Failed to configure YAML schema:', err);
    }

    // Lint the document on every change using our custom validator
    editor.onDidChangeModelContent(() => {
      const value = editor.getValue();
      const markers = validateMetricYaml(value, monacoInstance);
      monacoInstance.editor.setModelMarkers(editor.getModel()!, 'yaml', markers);
    });
  };

  return (
    <MonacoEditor
      width="800"
      height="600"
      language="yaml"
      className="h-full min-h-[600px] w-full min-w-[800px] border"
      theme="vs-light"
      value={initialValue}
      onMount={editorDidMount}
      options={{
        automaticLayout: true
      }}
    />
  );
};
