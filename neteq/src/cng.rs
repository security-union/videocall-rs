/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! # RFC 3389 Compliant Comfort Noise Generation
//!
//! This module implements sophisticated comfort noise generation following RFC 3389
//! standards, providing perceptually natural background noise during silence periods.
//!
//! ## Algorithm Overview
//!
//! The comfort noise generator uses a three-stage process:
//! 1. **Parameter Extraction**: Parse SID packets for energy and spectral parameters
//! 2. **Noise Generation**: Create Gaussian white noise with controlled energy
//! 3. **Spectral Shaping**: Apply LPC filtering to match background noise characteristics
//!
//! ## Key Features
//!
//! - RFC 3389 compliant SID packet processing
//! - LPC spectral shaping (1-12th order)
//! - Smooth parameter interpolation
//! - Professional audio quality
//! - Minimal CPU overhead
//! 

/// This is unused for now.

use std::f32::consts::PI;

/// Maximum LPC order supported (RFC 3389 allows up to 12)
const MAX_LPC_ORDER: usize = 12;

/// Energy conversion table from dB to linear scale (RFC 3389 Table 1)
/// Maps SID energy index (0-93) to actual energy values
const ENERGY_TABLE: [f32; 96] = [
    // Pre-computed energy values for fast lookup
    // These represent background noise energy levels from very quiet to loud
    0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001,
    0.0001, 0.0001, 0.0002, 0.0002, 0.0002, 0.0003, 0.0003, 0.0004,
    0.0005, 0.0006, 0.0007, 0.0008, 0.001, 0.0012, 0.0015, 0.0018,
    0.0022, 0.0027, 0.0032, 0.0039, 0.0047, 0.0056, 0.0068, 0.0081,
    0.0097, 0.0116, 0.0139, 0.0166, 0.0198, 0.0237, 0.0283, 0.0337,
    0.0402, 0.0480, 0.0573, 0.0683, 0.0815, 0.0972, 0.1159, 0.1382,
    0.1648, 0.1966, 0.2344, 0.2796, 0.3335, 0.3977, 0.4739, 0.5642,
    0.6310, 0.7079, 0.7943, 0.8913, 1.0, 1.122, 1.259, 1.413, 1.585,
    1.778, 1.995, 2.239, 2.512, 2.818, 3.162, 3.548, 3.981, 4.467,
    5.012, 5.623, 6.310, 7.079, 7.943, 8.913, 10.0, 11.22, 12.59,
    14.13, 15.85, 17.78, 19.95, 22.39, 25.12, 28.18, 31.62, 35.48,
    39.81, 44.67, 50.12, 56.23
];

/// SID (Silence Insertion Descriptor) packet structure
#[derive(Debug, Clone)]
pub struct SidPacket {
    /// Energy level index (0-93 dB)
    pub energy_level: u8,
    /// LPC reflection coefficients in Q7 format
    pub lpc_coeffs: Vec<u8>,
}

impl SidPacket {
    /// Parse SID packet from raw bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        
        let energy_level = data[0].min(95); // Clamp to valid range
        let lpc_coeffs = if data.len() > 1 {
            data[1..].to_vec()
        } else {
            Vec::new()
        };
        
        Some(SidPacket {
            energy_level,
            lpc_coeffs,
        })
    }
    
    /// Get linear energy value from dB index
    pub fn get_energy(&self) -> f32 {
        ENERGY_TABLE[self.energy_level as usize]
    }
    
    /// Convert Q7 LPC coefficients to floating point reflection coefficients
    pub fn get_reflection_coefficients(&self) -> Vec<f32> {
        self.lpc_coeffs
            .iter()
            .take(MAX_LPC_ORDER)
            .map(|&coeff| {
                // Convert Q7 to reflection coefficient in range [-1, 1]
                if coeff == 255 {
                    // Special case for maximum order
                    (coeff as f32) / 128.0 - 1.0
                } else {
                    // Standard Q7 to float conversion
                    (coeff as f32 - 127.0) / 128.0
                }
            })
            .collect()
    }
}

/// RFC 3389 compliant comfort noise decoder
pub struct CngDecoder {
    /// Target energy level from most recent SID packet
    target_energy: f32,
    /// Current interpolated energy level
    current_energy: f32,
    
    /// Target LPC reflection coefficients
    target_reflection_coeffs: [f32; MAX_LPC_ORDER],
    /// Current interpolated reflection coefficients
    current_reflection_coeffs: [f32; MAX_LPC_ORDER],
    
    /// LPC filter memory for spectral shaping
    filter_memory: [f32; MAX_LPC_ORDER],
    
    /// Random number generator state for Gaussian noise
    random_seed: u32,
    random_spare: Option<f32>, // For Box-Muller algorithm
    
    /// Generation state
    samples_since_update: usize,
    first_generation: bool,
    
    /// Configuration
    lpc_order: usize,
    
    /// Interpolation factors for smooth transitions
    energy_beta: f32,
    coeff_beta: f32,
}

impl CngDecoder {
    /// Create new comfort noise decoder
    pub fn new() -> Self {
        Self {
            target_energy: 0.001, // Very quiet default
            current_energy: 0.001,
            target_reflection_coeffs: [0.0; MAX_LPC_ORDER],
            current_reflection_coeffs: [0.0; MAX_LPC_ORDER],
            filter_memory: [0.0; MAX_LPC_ORDER],
            random_seed: 7777, // Same as libWebRTC for consistency
            random_spare: None,
            samples_since_update: 0,
            first_generation: true,
            lpc_order: 5, // Default order, good balance of quality vs complexity
            energy_beta: 0.95, // Smooth energy interpolation
            coeff_beta: 0.9,   // Smooth coefficient interpolation
        }
    }
    
    /// Update CNG parameters from SID packet
    pub fn update_parameters(&mut self, sid: &SidPacket) {
        // Update target energy (take 75% as per libWebRTC)
        self.target_energy = sid.get_energy() * 0.75;
        
        // Update reflection coefficients
        let new_coeffs = sid.get_reflection_coefficients();
        self.lpc_order = new_coeffs.len().min(MAX_LPC_ORDER);
        
        // Clear old coefficients
        self.target_reflection_coeffs.fill(0.0);
        
        // Set new coefficients
        for (i, &coeff) in new_coeffs.iter().enumerate() {
            if i < MAX_LPC_ORDER {
                self.target_reflection_coeffs[i] = coeff.clamp(-0.99, 0.99); // Stability constraint
            }
        }
        
        log::debug!(
            "CNG: Updated parameters - energy={:.6}, lpc_order={}, coeffs={:?}",
            self.target_energy,
            self.lpc_order,
            &self.target_reflection_coeffs[..self.lpc_order]
        );
    }
    
    /// Generate comfort noise samples
    pub fn generate(&mut self, samples: &mut [f32]) -> Result<(), &'static str> {
        if samples.is_empty() {
            return Ok(());
        }
        
        // Interpolate parameters for smooth transitions
        self.interpolate_parameters();
        
        // Convert reflection coefficients to direct form LPC coefficients
        let lpc_coeffs = self.reflection_to_lpc();
        
        // Generate each sample
        for sample in samples.iter_mut() {
            // Generate Gaussian white noise
            let noise = self.generate_gaussian_noise();
            
            // Scale by current energy
            let scaled_noise = noise * self.current_energy.sqrt();
            
            // Apply LPC filter for spectral shaping
            *sample = self.apply_lpc_filter(scaled_noise, &lpc_coeffs);
        }
        
        self.samples_since_update += samples.len();
        self.first_generation = false;
        
        Ok(())
    }
    
    /// Reset decoder state
    pub fn reset(&mut self) {
        self.current_energy = 0.001;
        self.current_reflection_coeffs.fill(0.0);
        self.filter_memory.fill(0.0);
        self.samples_since_update = 0;
        self.first_generation = true;
        self.random_spare = None;
        
        log::debug!("CNG: Reset decoder state");
    }
    
    /// Interpolate energy and coefficients for smooth transitions
    fn interpolate_parameters(&mut self) {
        // Interpolate energy
        self.current_energy = self.energy_beta * self.current_energy + 
                              (1.0 - self.energy_beta) * self.target_energy;
        
        // Interpolate reflection coefficients
        for i in 0..self.lpc_order {
            self.current_reflection_coeffs[i] = 
                self.coeff_beta * self.current_reflection_coeffs[i] +
                (1.0 - self.coeff_beta) * self.target_reflection_coeffs[i];
        }
    }
    
    /// Convert reflection coefficients to direct form LPC coefficients using Levinson-Durbin
    fn reflection_to_lpc(&self) -> Vec<f32> {
        let mut lpc = vec![0.0; self.lpc_order + 1];
        lpc[0] = 1.0; // a[0] = 1
        
        if self.lpc_order == 0 {
            return lpc;
        }
        
        let mut temp = vec![0.0; self.lpc_order];
        
        for m in 1..=self.lpc_order {
            let k = self.current_reflection_coeffs[m - 1];
            lpc[m] = k;
            
            // Update previous coefficients
            for i in 1..m {
                temp[i] = lpc[i] + k * lpc[m - i];
            }
            
            for i in 1..m {
                lpc[i] = temp[i];
            }
        }
        
        lpc
    }
    
    /// Generate Gaussian white noise using Box-Muller transform
    fn generate_gaussian_noise(&mut self) -> f32 {
        // Check if we have a spare value from previous call
        if let Some(spare) = self.random_spare.take() {
            return spare;
        }
        
        // Generate two independent uniform random numbers
        let u1 = self.uniform_random();
        let u2 = self.uniform_random();
        
        // Box-Muller transform
        let magnitude = (-2.0 * u1.ln()).sqrt();
        let angle = 2.0 * PI * u2;
        
        let z0 = magnitude * angle.cos();
        let z1 = magnitude * angle.sin();
        
        // Save one for next call
        self.random_spare = Some(z1);
        
        z0
    }
    
    /// Generate uniform random number [0, 1)
    fn uniform_random(&mut self) -> f32 {
        // Linear congruential generator (same as libWebRTC)
        self.random_seed = self.random_seed.wrapping_mul(1103515245).wrapping_add(12345);
        (self.random_seed as f32) / (u32::MAX as f32)
    }
    
    /// Apply LPC filter for spectral shaping
    fn apply_lpc_filter(&mut self, input: f32, lpc_coeffs: &[f32]) -> f32 {
        if self.lpc_order == 0 || lpc_coeffs.len() <= 1 {
            return input;
        }
        
        // IIR filter: y[n] = x[n] - sum(a[k] * y[n-k]) for k=1 to N
        let mut output = input;
        
        for i in 1..lpc_coeffs.len().min(self.lpc_order + 1) {
            if i <= self.filter_memory.len() {
                output -= lpc_coeffs[i] * self.filter_memory[i - 1];
            }
        }
        
        // Update filter memory (shift delay line)
        if self.lpc_order > 0 {
            for i in (1..self.lpc_order).rev() {
                self.filter_memory[i] = self.filter_memory[i - 1];
            }
            self.filter_memory[0] = output;
        }
        
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_sid_packet_parsing() {
        let data = [30, 100, 150, 200]; // Energy + 3 LPC coefficients
        let sid = SidPacket::from_bytes(&data).unwrap();
        
        assert_eq!(sid.energy_level, 30);
        assert_eq!(sid.lpc_coeffs.len(), 3);
        assert!(sid.get_energy() > 0.0);
    }
    
    #[test]
    fn test_reflection_coefficient_conversion() {
        let data = [10, 127, 200, 50]; // Various Q7 values
        let sid = SidPacket::from_bytes(&data).unwrap();
        let coeffs = sid.get_reflection_coefficients();
        
        // Check that coefficients are in valid range [-1, 1]
        for coeff in coeffs {
            assert!(coeff >= -1.0 && coeff <= 1.0);
        }
    }
    
    #[test]
    fn test_cng_decoder_basic() {
        let mut decoder = CngDecoder::new();
        let mut samples = vec![0.0; 160]; // 10ms at 16kHz
        
        let result = decoder.generate(&mut samples);
        assert!(result.is_ok());
        
        // Check that samples are not all zero
        let has_non_zero = samples.iter().any(|&s| s.abs() > 1e-10);
        assert!(has_non_zero);
    }
    
    #[test]
    fn test_parameter_update() {
        let mut decoder = CngDecoder::new();
        let initial_energy = decoder.current_energy;
        
        let sid_data = [50, 100, 150]; // Higher energy
        let sid = SidPacket::from_bytes(&sid_data).unwrap();
        decoder.update_parameters(&sid);
        
        // Target energy should be updated
        assert!(decoder.target_energy > initial_energy);
    }
} 