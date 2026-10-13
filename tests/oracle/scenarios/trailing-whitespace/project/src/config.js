// Default settings for the HTTP client.   
export const defaults = {   
  baseUrl: "https://api.example.com",  
  timeout: 30,    
  headers: {  
    "user-agent": "example-client/1.0", 
  },  
};   

// Settings for one client: the defaults, with what was passed on top.  
export function getConfig(overrides = {}) {  
  return { ...defaults, ...overrides, headers: { ...defaults.headers, ...overrides.headers } };  
}
